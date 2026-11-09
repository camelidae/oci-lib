// SPDX-License-Identifier: Apache-2.0

package main

// insecureRegistriesConf is the containers/image registries.conf used when
// insecure pulls are enabled. Prefixes are local names and RFC1918-style
// ranges only. Every entry has both prefix and location set; containers/image
// rejects a registry block when both are empty. `insecure` is what sets
// tls.Config.InsecureSkipVerify, which is required for the HTTPS→HTTP fallback.
func insecureRegistriesConf() []byte {
	return []byte("" +
		"unqualified-search-registries = [\"docker.io\"]\n" +
		"\n" +
		"[[registry]]\n" +
		"prefix = \"localhost\"\n" +
		"location = \"localhost\"\n" +
		"insecure = true\n" +
		"\n" +
		"[[registry]]\n" +
		"prefix = \"127.0.0.1\"\n" +
		"location = \"127.0.0.1\"\n" +
		"insecure = true\n" +
		"\n" +
		"[[registry]]\n" +
		"prefix = \"oci-registry\"\n" +
		"location = \"oci-registry\"\n" +
		"insecure = true\n" +
		"\n" +
		"[[registry]]\n" +
		"prefix = \"10.\"\n" +
		"location = \"10.\"\n" +
		"insecure = true\n" +
		"\n" +
		"[[registry]]\n" +
		"prefix = \"192.168.\"\n" +
		"location = \"192.168.\"\n" +
		"insecure = true\n" +
		"\n" +
		"[[registry]]\n" +
		"prefix = \"172.\"\n" +
		"location = \"172.\"\n" +
		"insecure = true\n")
}
