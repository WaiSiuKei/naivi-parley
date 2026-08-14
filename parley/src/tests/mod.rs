// Copyright 2024 the Parley Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// These suites assert harfrust-path behavior against bundled test fonts;
// the macOS CoreText backend replaces that path, so they only run off-macOS.
#[cfg(not(target_os = "macos"))]
mod test_analysis;
#[cfg(not(target_os = "macos"))]
mod test_builders;
mod utils;
