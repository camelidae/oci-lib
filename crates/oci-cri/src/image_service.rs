// SPDX-License-Identifier: Apache-2.0

//! CRI ImageService over the engine's containers/image+storage store.

use std::collections::HashMap;
use std::pin::Pin;

use tonic::{Request, Response, Status};

use crate::api::image_service_server::ImageService;
use crate::api::*;
use crate::engine::EngineImage;
use crate::service::CriService;

type BoxStream<T> =
    Pin<Box<dyn tonic::codegen::tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

fn to_image(image: &EngineImage) -> Image {
    Image {
        id: image.id.clone(),
        repo_tags: image.repo_tags.clone(),
        repo_digests: image.repo_digests.clone(),
        size: image.size,
        uid: image.uid.map(|value| Int64Value { value }),
        username: image.username.clone(),
        spec: image.spec.as_ref().map(|_spec| ImageSpec {
            image: image
                .repo_tags
                .first()
                .cloned()
                .unwrap_or_else(|| image.id.clone()),
            annotations: HashMap::new(),
            user_specified_image: image
                .repo_tags
                .first()
                .cloned()
                .unwrap_or_else(|| image.id.clone()),
            runtime_handler: String::new(),
            image_ref: String::new(),
        }),
        pinned: image.pinned,
    }
}

fn matches(image: &EngineImage, filter: Option<&ImageFilter>) -> bool {
    let Some(filter) = filter else { return true };
    let Some(wanted) = &filter.image else {
        return true;
    };
    wanted.image.is_empty() || image.matches_reference(&wanted.image)
}

#[tonic::async_trait]
impl ImageService for CriService {
    async fn list_images(
        &self,
        request: Request<ListImagesRequest>,
    ) -> Result<Response<ListImagesResponse>, Status> {
        let filter = request.into_inner().filter;
        let images = self
            .inner
            .engine
            .list_images("")
            .map_err(Status::from)?
            .iter()
            .filter(|image| matches(image, filter.as_ref()))
            .map(to_image)
            .collect();
        Ok(Response::new(ListImagesResponse { images }))
    }

    type StreamImagesStream = BoxStream<StreamImagesResponse>;

    async fn stream_images(
        &self,
        request: Request<StreamImagesRequest>,
    ) -> Result<Response<Self::StreamImagesStream>, Status> {
        let listed = self
            .list_images(Request::new(ListImagesRequest {
                filter: request.into_inner().filter,
            }))
            .await?
            .into_inner();
        let stream = tonic::codegen::tokio_stream::iter(vec![Ok(StreamImagesResponse {
            images: listed.images,
        })]);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn image_status(
        &self,
        request: Request<ImageStatusRequest>,
    ) -> Result<Response<ImageStatusResponse>, Status> {
        let request = request.into_inner();
        let spec = request.image.unwrap_or_default();
        let plugin = crate::runtime_plugin::resolve(&self.inner.config, &spec.runtime_handler)?;
        let wanted = spec.image;
        let found = self
            .inner
            .engine
            .list_images("")
            .map_err(Status::from)?
            .into_iter()
            .find(|image| {
                image.matches_reference(&wanted)
                    && crate::runtime_plugin::matches_image(plugin, image)
            });
        Ok(Response::new(ImageStatusResponse {
            image: found.as_ref().map(to_image),
            info: if request.verbose {
                found
                    .map(|image| {
                        HashMap::from([(
                            "image".to_string(),
                            serde_json::to_string(&image).unwrap_or_default(),
                        )])
                    })
                    .unwrap_or_default()
            } else {
                HashMap::new()
            },
        }))
    }

    async fn pull_image(
        &self,
        request: Request<PullImageRequest>,
    ) -> Result<Response<PullImageResponse>, Status> {
        let inner = std::sync::Arc::clone(&self.inner);
        let request = request.into_inner();
        let image_ref = tokio::task::spawn_blocking(move || -> crate::error::Result<String> {
            let spec = request.image.ok_or_else(|| {
                crate::Error::new(
                    crate::ErrorCode::InvalidArgument,
                    "pull request has no image",
                    "",
                )
            })?;
            let auth = request.auth.map(|a| crate::engine::Auth {
                username: a.username,
                password: a.password,
                auth: a.auth,
                server_address: a.server_address,
                identity_token: a.identity_token,
            });
            let image = crate::image_reference::normalize(&spec.image)?;
            let plugin = crate::runtime_plugin::resolve(&inner.config, &spec.runtime_handler)?;
            let id = inner.engine.pull_image_for_platform(
                &image,
                auth.as_ref(),
                inner.config.insecure_registries,
                plugin.map(|p| p.platform.as_str()),
            )?;
            let matches = inner.engine.list_images("")?.iter().any(|image| {
                image.matches_reference(&id) && crate::runtime_plugin::matches_image(plugin, image)
            });
            if !matches {
                return Err(crate::Error::new(
                    crate::ErrorCode::InvalidArgument,
                    "pulled image platform does not match runtime handler",
                    &spec.runtime_handler,
                ));
            }
            Ok(id)
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(PullImageResponse { image_ref }))
    }

    async fn remove_image(
        &self,
        request: Request<RemoveImageRequest>,
    ) -> Result<Response<RemoveImageResponse>, Status> {
        let wanted = request
            .into_inner()
            .image
            .map(|i| i.image)
            .unwrap_or_default();
        // Idempotent: removing an absent image is success.
        let present = self
            .inner
            .engine
            .list_images("")
            .map_err(Status::from)?
            .into_iter()
            .find(|image| image.matches_reference(&wanted));
        if let Some(image) = present {
            self.inner
                .engine
                .remove_image(&image.id)
                .map_err(Status::from)?;
        }
        Ok(Response::new(RemoveImageResponse {}))
    }

    async fn image_fs_info(
        &self,
        _request: Request<ImageFsInfoRequest>,
    ) -> Result<Response<ImageFsInfoResponse>, Status> {
        let filesystems = self
            .inner
            .engine
            .image_fs_info()
            .map_err(Status::from)?
            .into_iter()
            .map(|fs| FilesystemUsage {
                timestamp: fs.timestamp_ns,
                fs_id: Some(FilesystemIdentifier {
                    mountpoint: fs.mountpoint,
                }),
                used_bytes: Some(UInt64Value {
                    value: fs.used_bytes,
                }),
                inodes_used: Some(UInt64Value {
                    value: fs.inodes_used,
                }),
            })
            .collect::<Vec<_>>();
        Ok(Response::new(ImageFsInfoResponse {
            image_filesystems: filesystems.clone(),
            container_filesystems: filesystems,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tag: &str) -> EngineImage {
        EngineImage {
            id: "sha256:abc".into(),
            repo_tags: vec![tag.into()],
            repo_digests: vec![format!("{tag}@sha256:abc")],
            size: 42,
            ..EngineImage::default()
        }
    }

    #[test]
    fn untagged_image_lookup_uses_latest() {
        let image = sample("registry.example/app:latest");
        assert!(matches(
            &image,
            Some(&ImageFilter {
                image: Some(ImageSpec {
                    image: "registry.example/app".into(),
                    ..Default::default()
                })
            })
        ));
    }

    #[test]
    fn short_lookup_matches_tags_and_digests_only_in_the_default_registry() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let image = EngineImage {
            id: "local-id".into(),
            repo_tags: vec!["docker.io/library/busybox:1.36".into()],
            repo_digests: vec![format!("docker.io/library/busybox@{digest}")],
            ..Default::default()
        };
        for reference in [
            "local-id".to_string(),
            "busybox:1.36".to_string(),
            format!("busybox@{digest}"),
            format!("busybox:1.36@{digest}"),
        ] {
            assert!(image.matches_reference(&reference), "{reference}");
        }
        for reference in [
            "busybox",
            "busybox:other",
            "registry.example/busybox:1.36",
            "",
        ] {
            assert!(!image.matches_reference(reference), "{reference}");
        }
    }

    #[test]
    fn filter_matches_id_tag_or_digest() {
        let image = sample("example/app:latest");
        assert!(matches(&image, None));
        assert!(matches(
            &image,
            Some(&ImageFilter {
                image: Some(ImageSpec {
                    image: "sha256:abc".into(),
                    ..ImageSpec::default()
                }),
            })
        ));
        assert!(matches(
            &image,
            Some(&ImageFilter {
                image: Some(ImageSpec {
                    image: "example/app:latest".into(),
                    ..ImageSpec::default()
                }),
            })
        ));
        assert!(matches(
            &image,
            Some(&ImageFilter {
                image: Some(ImageSpec {
                    image: "example/app:latest@sha256:abc".into(),
                    ..ImageSpec::default()
                }),
            })
        ));
        assert!(!matches(
            &image,
            Some(&ImageFilter {
                image: Some(ImageSpec {
                    image: "other:latest".into(),
                    ..ImageSpec::default()
                }),
            })
        ));
    }

    #[test]
    fn to_image_fills_spec_from_first_tag() {
        let mut engine_image = sample("registry.example/app:1");
        engine_image.spec = Some(crate::engine::ImageSpec::default());
        let image = to_image(&engine_image);
        assert_eq!(image.id, "sha256:abc");
        assert_eq!(image.size, 42);
        assert_eq!(image.repo_tags, ["registry.example/app:1"]);
        assert_eq!(image.spec.as_ref().unwrap().image, "registry.example/app:1");
    }
}
