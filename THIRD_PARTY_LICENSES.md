# Third-Party Code Attribution

PAWA Pipeline uses the following third-party libraries, tools, runtime components and game software. This document records the components identified in this repository.

Rust crate versions and declared licenses are taken from [Cargo.lock](Cargo.lock) through `cargo metadata --locked --filter-platform x86_64-unknown-linux-gnu` (384 third-party crates on the Linux target). Runtime components are taken from [docker/Dockerfile](docker/Dockerfile) and `crates/capture/src/warmup.rs`, which download them at image-build time; their licenses are given from upstream project information and are marked **(upstream, not re-verified)** where this inventory could not read the license file of the exact version. No third-party binaries, jars, game files or crates are included in this repository.

---

## 1. Azalea (Headless Minecraft Client Library)

**Crates:** `azalea`, `azalea-protocol`, `azalea-entity` and their `azalea-*` sub-crates  
**Version:** `0.16.0+mc26.2`, git revision `6249c295d353b9b3ef68f665b311cba39211fd19`  
**Source:** Azalea contributors  
**Repository:** [azalea-rs/azalea](https://github.com/azalea-rs/azalea)  
**License:** MIT (declared in crate metadata)  
**Usage:** The player runtime (`crates/players`) connects scripted players to the Fabric server, drives movement, camera and block interaction, and reads world-age time and entity physics.

Azalea pulls in a pre-release crypto dependency chain (`azalea-auth`) and `socks5-impl`; see section 2 and the license summary.

---

## 2. socks5-impl (Transitive Dependency of azalea-protocol)

**Crate:** `socks5-impl`  
**Version:** `0.9.6`  
**Repository:** [ssrlive/socks5-impl](https://github.com/ssrlive/socks5-impl)  
**License:** **GPL-3.0-or-later** (crate metadata and bundled `LICENSE`)  
**Usage:** Non-optional dependency of `azalea-protocol` (feature `client`), used by Azalea for SOCKS5 proxy connections. PAWA code does not call it directly, but it is compiled and linked into the `players` binary.

Distributing the source code under Apache-2.0 is unaffected, because the crate is not included in this repository. A distributed `players` binary (for example inside the published Docker image) links GPL-3.0 code; such a distribution must satisfy GPL-3.0 for the combined binary (source availability, license notice). Options: keep binaries/images undistributed, comply with GPL-3.0 for the binary, or remove the dependency through an upstream Azalea feature flag or a patch.

---

## 3. Other Rust Crates

All remaining crates are listed in [Appendix A](#appendix-a-rust-crate-inventory). Declared license families (count of crates):

| Declared license | Crates |
| --- | ---: |
| MIT OR Apache-2.0 (incl. `MIT/Apache-2.0`, `Apache-2.0 OR MIT` spellings) | 247 |
| MIT | 78 |
| Unicode-3.0 (ICU4X family: `icu_*`, `zerovec`, `yoke`, `tinystr`, ...) | 18 |
| Apache-2.0 | 4 |
| ISC / ISC-including (`rustls-webpki`, `untrusted`, `simple_asn1`, `rsa_public_encrypt_pkcs1`, `ring`, `aws-lc-rs`, `aws-lc-sys`) | 7 |
| Zlib (`foldhash`, `slotmap`, `zlib-rs`) | 3 |
| BSD-3-Clause (`subtle`, `tiny-skia`, `tiny-skia-path`) | 3 |
| BSD-2-Clause (`arrayref`) | 1 |
| Unlicense (`minecraft_folder_path`) | 1 |
| Other expressions combining permissive licenses (Zlib, Unlicense, BSL-1.0, CC0, 0BSD, ISC, Unicode-3.0 with MIT/Apache-2.0) | 21 |
| GPL-3.0-or-later (`socks5-impl`, section 2) | 1 |

`aws-lc-sys` and `ring` compile bundled C code (AWS-LC / BoringSSL derivatives) whose notices include OpenSSL/SSLeay-style and ISC terms; preserve their `LICENSE` files when distributing binaries. `resvg`/`tiny-skia` (HUD overlay) are BSD-3-Clause / Apache-2.0 OR MIT.

---

## 4. Minecraft (Game Client, Server and Assets)

**Source:** Mojang Studios / Microsoft  
**Version:** `26.2`  
**License:** Proprietary. Use is governed by the [Minecraft EULA](https://www.minecraft.net/eula) and the [Minecraft Usage Guidelines](https://www.minecraft.net/usage-guidelines)  
**Usage:** The Fabric server runs the vanilla dedicated server; the renderer runs the Minecraft client through HeadlessMC to render replays. Both are downloaded from Mojang at image-build time (`pawa warmup-render`, `pawa warmup-server`). The capture stage writes `eula=true` for the server it starts.

The source repository contains no Minecraft code or assets. **The runtime image built by `docker/Dockerfile` contains the Minecraft client, its asset objects and the server libraries**, so publishing that image publicly would redistribute Mojang software. Rendered dataset videos contain Minecraft game visuals. The README states that the project is not affiliated with Mojang or Microsoft.

---

## 5. Fabric Loader, Fabric API, Fabric Language Kotlin, Sponge Mixin

**Versions:** Fabric Loader `0.19.3` (server launcher `1.1.2`), Fabric API `0.157.0+26.2`, Fabric Language Kotlin `1.13.13+kotlin.2.4.10`, Sponge Mixin `0.17.3+mixin.0.8.7`  
**Repositories:** [FabricMC/fabric-loader](https://github.com/FabricMC/fabric-loader), [FabricMC/fabric](https://github.com/FabricMC/fabric), [FabricMC/fabric-language-kotlin](https://github.com/FabricMC/fabric-language-kotlin), [FabricMC/Mixin](https://github.com/FabricMC/Mixin)  
**License:** Apache-2.0 (Fabric Loader, Fabric API, Fabric Language Kotlin); MIT (Sponge Mixin) **(upstream, not re-verified)**  
**Usage:** Mod loader and APIs for the capture server and render client. Fabric Loader and Mixin are compile-time classpath entries for the driver mod; they are not bundled into `rmdriver.jar`.

---

## 6. ReplayMod (Replay Rendering)

**Version:** `replaymod-26.2-2.6.27` (Modrinth)  
**Repository:** [ReplayMod/ReplayMod](https://github.com/ReplayMod/ReplayMod)  
**License:** GPL-3.0 **(upstream, not re-verified)**  
**Usage:** Renders saved `.mcpr` replays into POV, equirectangular, depth and observer videos.

The project's own driver mod (`assets/renderer/replaymod-driver-mod`, Apache-2.0) does not compile against ReplayMod; it calls it by runtime reflection and its mixins target Minecraft classes only. The Dockerfile, however, **modifies the ReplayMod jar** in the image (it deletes two mixin entries, `Mixin_SkipHudDuringRender` and `MixinInGameHud`, so the HUD stays visible). The modification is fully described in the Dockerfile. Distributing the image would distribute a modified GPL-3.0 work and needs GPL-3.0 compliance; whether a reflection-based companion mod is a separate work under GPL-3.0 should be confirmed by legal review.

---

## 7. ServerReplay (Server-Side Replay Recording)

**Version:** `ServerReplay-3.5.2+26.2` (Modrinth)  
**Repository:** [senseiwells/ServerReplay](https://github.com/senseiwells/ServerReplay)  
**License:** MPL-2.0 **(upstream, not re-verified; confirm against the release)**  
**Usage:** Records ReplayMod-compatible replays of each player on the capture server.

---

## 8. HeadlessMC

**Version:** `2.10.0` (`headlessmc-launcher-wrapper`)  
**Repository:** [headlesshq/headlessmc](https://github.com/headlesshq/headlessmc)  
**License:** MIT **(upstream, not re-verified)**  
**Usage:** Launches the Minecraft client without a launcher UI for rendering.

---

## 9. HyperQueue (Task Scheduling)

**Version:** source commit `680857bdfa45f92c50535cdef525f5a26bbcf55c`, patched with [docker/hq-taskqueue-panic.patch](docker/hq-taskqueue-panic.patch)  
**Repository:** [It4innovations/hyperqueue](https://github.com/It4innovations/hyperqueue)  
**License:** MIT **(upstream, not re-verified)**  
**Usage:** Schedules capture, render and finalize units across CPU and GPU hosts.

The patch file is a 3-line modification of HyperQueue's `crates/tako/src/internal/server/core.rs` authored for this project. It is a derivative of MIT-licensed code; keep HyperQueue's copyright notice with it.

---

## 10. VirtualGL, Mesa, Xvfb

**Versions:** VirtualGL `3.1.5` (upstream `.deb`); Mesa and Xvfb from Debian trixie  
**License:** VirtualGL: wxWindows Library Licence 3.1 (LGPL-2.1 with exceptions); Mesa: MIT and related permissive licenses; Xvfb: MIT/X11 **(upstream, not re-verified)**  
**Usage:** Off-screen OpenGL rendering on GPU (EGL) or in software (llvmpipe).

---

## 11. FFmpeg

**Source:** Debian `ffmpeg` package (runtime image, CI); invoked as a separate process  
**License:** LGPL-2.1-or-later, with GPL-2.0-or-later parts enabled in the Debian build (e.g. `libx264`) **(upstream, not re-verified)**; see [FFmpeg legal](https://ffmpeg.org/legal.html)  
**Usage:** H.264 encoding, frame-exact re-encoding, probing, HUD video composition. The `ffmpeg-sidecar` crate (MIT) wraps the process; its auto-download feature is disabled.

---

## 12. OpenJDK, Gson, Kotlin runtime

**Versions:** OpenJDK 25 (Debian), Gson `2.14.0` (compile-time only), Kotlin stdlib via Fabric Language Kotlin  
**License:** OpenJDK: GPL-2.0 with Classpath Exception; Gson: Apache-2.0; Kotlin: Apache-2.0 **(upstream, not re-verified)**  
**Usage:** Java runtime for the server and client; Gson is resolved from Minecraft's own libraries at runtime and not bundled.

---

## 13. Python Libraries (ops scripts only)

`ops/*.py` use the Python standard library plus, in individual audit scripts, **Pillow** (MIT-CMU / HPND), **requests** (Apache-2.0) and **huggingface_hub** (Apache-2.0). Versions are not pinned. These scripts are optional tooling and are not part of the runtime image.

---

## 14. Build and CI Tools (not distributed)

Rust toolchain (MIT OR Apache-2.0), `just` (CC0-1.0), Debian base image (`debian:trixie-slim`, mixed free licenses), GitHub Actions (`actions/checkout`, `docker/*`, `Swatinem/rust-cache`, `taiki-e/install-action`; MIT / Apache-2.0).

---

## 15. Data and Formats Referenced

- **Solaris multiplayer dataset format** (Savva et al., 2026): PAWA's episode layout is a superset of it for compatibility. Only the file layout is followed; no Solaris code or data is included.
- **Google Spherical Video V1 metadata** (`crates/render/src/spherical.rs`): an XML metadata format written into the panorama MP4s; no third-party code.

---

## License Summary

| Component | License or status |
| --- | --- |
| PAWA Pipeline original code (Rust, Java driver mod, Python, shell, viewer) | Apache-2.0 (`LICENSE`, `NOTICE`) |
| Azalea, most Rust crates | MIT / MIT OR Apache-2.0 |
| ICU4X crates | Unicode-3.0 |
| ring, aws-lc, rustls-webpki and related | ISC / Apache-2.0 / OpenSSL-style notices |
| socks5-impl (linked into `players` binary) | **GPL-3.0-or-later** |
| Fabric Loader / API / Language Kotlin, Gson, Kotlin | Apache-2.0 |
| Sponge Mixin, HeadlessMC, HyperQueue, Mesa | MIT |
| ReplayMod (modified inside the runtime image) | **GPL-3.0** |
| ServerReplay | MPL-2.0 (to confirm) |
| VirtualGL | wxWindows Library Licence 3.1 |
| FFmpeg (Debian build) | LGPL / GPL, build-dependent |
| OpenJDK | GPL-2.0 with Classpath Exception |
| Minecraft client, server and assets | **Proprietary (Minecraft EULA)**; downloaded at build time, present in the runtime image |

This repository contains only original code, the HyperQueue patch and configuration; it contains no third-party source files, binaries or game assets. Copyleft and proprietary obligations arise only when **binaries or the runtime image** are distributed.

---

## Legal Notice

- Third-party components remain subject to their respective licenses and copyright notices.
- This attribution inventory does not replace the full license texts, required notices, or source-distribution obligations of components that are redistributed.
- The Apache-2.0 license adopted for original PAWA Pipeline code does not relicense third-party code, Minecraft software or assets, or the dataset.
- Minecraft is a trademark of Mojang Synergies AB. This project is not affiliated with Mojang or Microsoft.
- Update this document when `Cargo.lock`, the Dockerfile or the downloaded component versions change.

---

## Appendix A. Rust Crate Inventory

Generated from `Cargo.lock` with `cargo metadata --locked --filter-platform x86_64-unknown-linux-gnu`. Workspace crates (`pawa`, `pawa-*`, `players`) are excluded. Licenses reproduce crate metadata.

| Crate | Version | License (as declared) | Source |
| --- | --- | --- | --- |
| `adler2` | 2.0.1 | 0BSD OR MIT OR Apache-2.0 | crates.io |
| `aes` | 0.9.2 | MIT OR Apache-2.0 | crates.io |
| `aho-corasick` | 1.1.5 | Unlicense OR MIT | crates.io |
| `anstream` | 1.0.0 | MIT OR Apache-2.0 | crates.io |
| `anstyle` | 1.0.14 | MIT OR Apache-2.0 | crates.io |
| `anstyle-parse` | 1.0.0 | MIT OR Apache-2.0 | crates.io |
| `anstyle-query` | 1.1.5 | MIT OR Apache-2.0 | crates.io |
| `anyhow` | 1.0.104 | MIT OR Apache-2.0 | crates.io |
| `arrayref` | 0.3.9 | BSD-2-Clause | crates.io |
| `arrayvec` | 0.7.8 | MIT OR Apache-2.0 | crates.io |
| `assert_type_match` | 0.1.1 | MIT OR Apache-2.0 | crates.io |
| `async-channel` | 2.5.0 | Apache-2.0 OR MIT | crates.io |
| `async-compat` | 0.2.5 | Apache-2.0 OR MIT | crates.io |
| `async-executor` | 1.14.0 | Apache-2.0 OR MIT | crates.io |
| `async-task` | 4.7.1 | Apache-2.0 OR MIT | crates.io |
| `async-trait` | 0.1.92 | MIT OR Apache-2.0 | crates.io |
| `atomic-waker` | 1.1.2 | Apache-2.0 OR MIT | crates.io |
| `autocfg` | 1.5.1 | Apache-2.0 OR MIT | crates.io |
| `aws-lc-rs` | 1.18.0 | ISC AND (Apache-2.0 OR ISC) | crates.io |
| `aws-lc-sys` | 0.44.0 | ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0) | crates.io |
| `azalea` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-auth` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-block` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-block-macros` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-brigadier` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-buf` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-buf-macros` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-chat` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-client` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-core` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-crypto` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-entity` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-inventory` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-inventory-macros` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-language` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-physics` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-protocol` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-protocol-macros` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-registry` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-registry-macros` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `azalea-world` | 0.16.0+mc26.2 | MIT | git (azalea-rs/azalea @ 6249c295) |
| `backon` | 1.6.0 | Apache-2.0 | crates.io |
| `base16ct` | 1.0.0 | Apache-2.0 OR MIT | crates.io |
| `base64` | 0.22.1 | MIT OR Apache-2.0 | crates.io |
| `base64` | 0.23.1 | MIT OR Apache-2.0 | crates.io |
| `base64ct` | 1.8.3 | Apache-2.0 OR MIT | crates.io |
| `bevy_app` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_derive` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_ecs` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_ecs_macro_logic` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_ecs_macros` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_log` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_macro_utils` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_platform` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_ptr` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_reflect` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_reflect_derive` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_tasks` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_time` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bevy_utils` | 0.19.1 | MIT OR Apache-2.0 | crates.io |
| `bitflags` | 2.13.1 | MIT OR Apache-2.0 | crates.io |
| `block-buffer` | 0.10.4 | MIT OR Apache-2.0 | crates.io |
| `block-buffer` | 0.12.1 | MIT OR Apache-2.0 | crates.io |
| `bumpalo` | 3.20.3 | MIT OR Apache-2.0 | crates.io |
| `bytemuck` | 1.25.2 | Zlib OR Apache-2.0 OR MIT | crates.io |
| `bytemuck_derive` | 1.12.0 | Zlib OR Apache-2.0 OR MIT | crates.io |
| `byteorder` | 1.5.0 | Unlicense OR MIT | crates.io |
| `bytes` | 1.12.1 | MIT | crates.io |
| `castaway` | 0.2.4 | MIT | crates.io |
| `cc` | 1.4.2 | MIT OR Apache-2.0 | crates.io |
| `cfb8` | 0.9.1 | MIT OR Apache-2.0 | crates.io |
| `cfg-if` | 1.0.4 | MIT OR Apache-2.0 | crates.io |
| `cfg_aliases` | 0.2.2 | MIT | crates.io |
| `chacha20` | 0.10.1 | MIT OR Apache-2.0 | crates.io |
| `chrono` | 0.4.45 | MIT OR Apache-2.0 | crates.io |
| `cipher` | 0.5.2 | MIT OR Apache-2.0 | crates.io |
| `clap` | 4.6.6 | MIT OR Apache-2.0 | crates.io |
| `clap_builder` | 4.6.6 | MIT OR Apache-2.0 | crates.io |
| `clap_derive` | 4.6.4 | MIT OR Apache-2.0 | crates.io |
| `clap_lex` | 1.1.0 | MIT OR Apache-2.0 | crates.io |
| `cmake` | 0.1.58 | MIT OR Apache-2.0 | crates.io |
| `cmov` | 0.5.4 | Apache-2.0 OR MIT | crates.io |
| `colorchoice` | 1.0.5 | MIT OR Apache-2.0 | crates.io |
| `compact_str` | 0.9.1 | MIT | crates.io |
| `concurrent-queue` | 2.5.0 | Apache-2.0 OR MIT | crates.io |
| `console` | 0.16.4 | MIT | crates.io |
| `const-oid` | 0.10.2 | Apache-2.0 OR MIT | crates.io |
| `convert_case` | 0.10.0 | MIT | crates.io |
| `cpubits` | 0.1.1 | MIT OR Apache-2.0 | crates.io |
| `cpufeatures` | 0.2.17 | MIT OR Apache-2.0 | crates.io |
| `cpufeatures` | 0.3.0 | MIT OR Apache-2.0 | crates.io |
| `crc32c` | 0.6.8 | Apache-2.0/MIT | crates.io |
| `crc32fast` | 1.5.0 | MIT OR Apache-2.0 | crates.io |
| `critical-section` | 1.2.0 | MIT OR Apache-2.0 | crates.io |
| `crossbeam-channel` | 0.5.16 | MIT OR Apache-2.0 | crates.io |
| `crossbeam-epoch` | 0.9.20 | MIT OR Apache-2.0 | crates.io |
| `crossbeam-queue` | 0.3.13 | MIT OR Apache-2.0 | crates.io |
| `crossbeam-utils` | 0.8.22 | MIT OR Apache-2.0 | crates.io |
| `crypto-bigint` | 0.7.5 | Apache-2.0 OR MIT | crates.io |
| `crypto-common` | 0.1.7 | MIT OR Apache-2.0 | crates.io |
| `crypto-common` | 0.2.2 | MIT OR Apache-2.0 | crates.io |
| `crypto-primes` | 0.7.2 | Apache-2.0 OR MIT | crates.io |
| `ctrlc` | 3.5.2 | MIT/Apache-2.0 | crates.io |
| `ctutils` | 0.4.2 | Apache-2.0 OR MIT | crates.io |
| `data-encoding` | 2.11.1 | MIT | crates.io |
| `data-url` | 0.3.2 | MIT OR Apache-2.0 | crates.io |
| `der` | 0.8.1 | Apache-2.0 OR MIT | crates.io |
| `derive_more` | 2.1.1 | MIT | crates.io |
| `derive_more-impl` | 2.1.1 | MIT | crates.io |
| `digest` | 0.10.7 | MIT OR Apache-2.0 | crates.io |
| `digest` | 0.11.3 | MIT OR Apache-2.0 | crates.io |
| `displaydoc` | 0.2.7 | MIT OR Apache-2.0 | crates.io |
| `disqualified` | 1.0.0 | MIT OR Apache-2.0 | crates.io |
| `downcast-rs` | 2.0.2 | MIT OR Apache-2.0 | crates.io |
| `dunce` | 1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 | crates.io |
| `dyn-clone` | 1.0.20 | MIT OR Apache-2.0 | crates.io |
| `either` | 1.17.0 | MIT OR Apache-2.0 | crates.io |
| `enum-as-inner` | 0.7.0 | MIT/Apache-2.0 | crates.io |
| `equivalent` | 1.0.2 | Apache-2.0 OR MIT | crates.io |
| `erased-serde` | 0.4.10 | MIT OR Apache-2.0 | crates.io |
| `errno` | 0.3.14 | MIT OR Apache-2.0 | crates.io |
| `euclid` | 0.22.14 | MIT OR Apache-2.0 | crates.io |
| `event-listener` | 5.4.2 | Apache-2.0 OR MIT | crates.io |
| `event-listener-strategy` | 0.5.4 | Apache-2.0 OR MIT | crates.io |
| `fastrand` | 2.5.0 | Apache-2.0 OR MIT | crates.io |
| `fdeflate` | 0.3.7 | MIT OR Apache-2.0 | crates.io |
| `ffmpeg-sidecar` | 2.5.2 | MIT | crates.io |
| `find-msvc-tools` | 0.1.10 | MIT OR Apache-2.0 | crates.io |
| `fixedbitset` | 0.5.7 | MIT OR Apache-2.0 | crates.io |
| `flate2` | 1.1.9 | MIT OR Apache-2.0 | crates.io |
| `float-cmp` | 0.9.0 | MIT | crates.io |
| `foldhash` | 0.2.0 | Zlib | crates.io |
| `font-types` | 0.12.3 | MIT OR Apache-2.0 | crates.io |
| `fontconfig-parser` | 0.5.8 | MIT | crates.io |
| `fontdb` | 0.24.0 | MIT | crates.io |
| `form_urlencoded` | 1.2.2 | MIT OR Apache-2.0 | crates.io |
| `fs_extra` | 1.3.0 | MIT | crates.io |
| `futures` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-channel` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-core` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-executor` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-io` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-lite` | 2.6.1 | Apache-2.0 OR MIT | crates.io |
| `futures-macro` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-sink` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-task` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `futures-util` | 0.3.34 | MIT OR Apache-2.0 | crates.io |
| `generic-array` | 0.14.7 | MIT | crates.io |
| `getrandom` | 0.2.17 | MIT OR Apache-2.0 | crates.io |
| `getrandom` | 0.4.3 | MIT OR Apache-2.0 | crates.io |
| `glam` | 0.32.1 | MIT OR Apache-2.0 | crates.io |
| `harfrust` | 0.12.0 | MIT | crates.io |
| `hashbrown` | 0.16.1 | MIT OR Apache-2.0 | crates.io |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 | crates.io |
| `heck` | 0.5.0 | MIT OR Apache-2.0 | crates.io |
| `hex` | 0.4.3 | MIT OR Apache-2.0 | crates.io |
| `hickory-net` | 0.26.1 | MIT OR Apache-2.0 | crates.io |
| `hickory-proto` | 0.26.1 | MIT OR Apache-2.0 | crates.io |
| `hickory-resolver` | 0.26.1 | MIT OR Apache-2.0 | crates.io |
| `http` | 1.5.0 | MIT OR Apache-2.0 | crates.io |
| `http-body` | 1.1.0 | MIT | crates.io |
| `http-body-util` | 0.1.5 | MIT | crates.io |
| `httparse` | 1.10.1 | MIT OR Apache-2.0 | crates.io |
| `hybrid-array` | 0.4.14 | MIT OR Apache-2.0 | crates.io |
| `hyper` | 1.11.0 | MIT | crates.io |
| `hyper-rustls` | 0.27.9 | Apache-2.0 OR ISC OR MIT | crates.io |
| `hyper-util` | 0.1.20 | MIT | crates.io |
| `iana-time-zone` | 0.1.65 | MIT OR Apache-2.0 | crates.io |
| `icu_collections` | 2.2.0 | Unicode-3.0 | crates.io |
| `icu_locale_core` | 2.2.0 | Unicode-3.0 | crates.io |
| `icu_normalizer` | 2.2.0 | Unicode-3.0 | crates.io |
| `icu_normalizer_data` | 2.2.0 | Unicode-3.0 | crates.io |
| `icu_properties` | 2.2.0 | Unicode-3.0 | crates.io |
| `icu_properties_data` | 2.2.0 | Unicode-3.0 | crates.io |
| `icu_provider` | 2.2.0 | Unicode-3.0 | crates.io |
| `idna` | 1.1.0 | MIT OR Apache-2.0 | crates.io |
| `idna_adapter` | 1.2.2 | Apache-2.0 OR MIT | crates.io |
| `imagesize` | 0.15.0 | MIT | crates.io |
| `indexmap` | 2.14.0 | Apache-2.0 OR MIT | crates.io |
| `inout` | 0.2.2 | MIT OR Apache-2.0 | crates.io |
| `insta` | 1.48.0 | Apache-2.0 | crates.io |
| `ipnet` | 2.12.1 | MIT OR Apache-2.0 | crates.io |
| `is_terminal_polyfill` | 1.70.2 | MIT OR Apache-2.0 | crates.io |
| `itoa` | 1.0.18 | MIT OR Apache-2.0 | crates.io |
| `jobserver` | 0.1.35 | MIT OR Apache-2.0 | crates.io |
| `kurbo` | 0.13.1 | Apache-2.0 OR MIT | crates.io |
| `lazy_static` | 1.5.0 | MIT OR Apache-2.0 | crates.io |
| `libc` | 0.2.189 | MIT OR Apache-2.0 | crates.io |
| `linux-raw-sys` | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | crates.io |
| `litemap` | 0.8.2 | Unicode-3.0 | crates.io |
| `lock_api` | 0.4.14 | MIT OR Apache-2.0 | crates.io |
| `log` | 0.4.33 | MIT OR Apache-2.0 | crates.io |
| `lru-slab` | 0.1.2 | MIT OR Apache-2.0 OR Zlib | crates.io |
| `matchers` | 0.2.0 | MIT | crates.io |
| `md-5` | 0.10.6 | MIT OR Apache-2.0 | crates.io |
| `md-5` | 0.11.0 | MIT OR Apache-2.0 | crates.io |
| `memchr` | 2.8.3 | Unlicense OR MIT | crates.io |
| `memmap2` | 0.9.11 | MIT OR Apache-2.0 | crates.io |
| `minecraft_folder_path` | 0.1.2 | Unlicense | crates.io |
| `miniz_oxide` | 0.8.9 | MIT OR Zlib OR Apache-2.0 | crates.io |
| `mio` | 1.2.2 | MIT | crates.io |
| `moka` | 0.12.16 | (MIT OR Apache-2.0) AND Apache-2.0 | crates.io |
| `nix` | 0.31.3 | MIT | crates.io |
| `nohash-hasher` | 0.2.0 | Apache-2.0 OR MIT | crates.io |
| `nonmax` | 0.5.5 | MIT OR Apache-2.0 | crates.io |
| `nu-ansi-term` | 0.50.3 | MIT | crates.io |
| `num` | 0.4.3 | MIT OR Apache-2.0 | crates.io |
| `num-bigint` | 0.4.8 | MIT OR Apache-2.0 | crates.io |
| `num-bigint` | 0.5.1 | MIT OR Apache-2.0 | crates.io |
| `num-complex` | 0.4.6 | MIT OR Apache-2.0 | crates.io |
| `num-format` | 0.4.4 | MIT/Apache-2.0 | crates.io |
| `num-integer` | 0.1.47 | MIT OR Apache-2.0 | crates.io |
| `num-iter` | 0.1.46 | MIT OR Apache-2.0 | crates.io |
| `num-rational` | 0.4.2 | MIT OR Apache-2.0 | crates.io |
| `num-traits` | 0.2.19 | MIT OR Apache-2.0 | crates.io |
| `once_cell` | 1.21.4 | MIT OR Apache-2.0 | crates.io |
| `openssl-probe` | 0.2.1 | MIT OR Apache-2.0 | crates.io |
| `parking` | 2.2.1 | Apache-2.0 OR MIT | crates.io |
| `parking_lot` | 0.12.5 | MIT OR Apache-2.0 | crates.io |
| `parking_lot_core` | 0.9.12 | MIT OR Apache-2.0 | crates.io |
| `pem-rfc7468` | 1.0.0 | Apache-2.0 OR MIT | crates.io |
| `percent-encoding` | 2.3.2 | MIT OR Apache-2.0 | crates.io |
| `pico-args` | 0.5.0 | MIT | crates.io |
| `pin-project-lite` | 0.2.17 | Apache-2.0 OR MIT | crates.io |
| `pkcs1` | 0.8.0-rc.4 | Apache-2.0 OR MIT | crates.io |
| `pkcs8` | 0.11.0 | Apache-2.0 OR MIT | crates.io |
| `pkg-config` | 0.3.33 | MIT OR Apache-2.0 | crates.io |
| `png` | 0.18.1 | MIT OR Apache-2.0 | crates.io |
| `polycool` | 0.4.0 | MIT OR Apache-2.0 | crates.io |
| `portable-atomic` | 1.15.0 | Apache-2.0 OR MIT | crates.io |
| `potential_utf` | 0.1.5 | Unicode-3.0 | crates.io |
| `ppv-lite86` | 0.2.21 | MIT OR Apache-2.0 | crates.io |
| `prefix-trie` | 0.8.4 | MIT OR Apache-2.0 | crates.io |
| `proc-macro2` | 1.0.107 | MIT OR Apache-2.0 | crates.io |
| `process-wrap` | 9.1.0 | Apache-2.0 OR MIT | crates.io |
| `quinn` | 0.11.11 | MIT OR Apache-2.0 | crates.io |
| `quinn-proto` | 0.11.16 | MIT OR Apache-2.0 | crates.io |
| `quinn-udp` | 0.5.15 | MIT OR Apache-2.0 | crates.io |
| `quote` | 1.0.47 | MIT OR Apache-2.0 | crates.io |
| `radix-heap` | 0.4.2 | MIT | crates.io |
| `rand` | 0.10.2 | MIT OR Apache-2.0 | crates.io |
| `rand` | 0.8.7 | MIT OR Apache-2.0 | crates.io |
| `rand_chacha` | 0.3.1 | MIT OR Apache-2.0 | crates.io |
| `rand_core` | 0.10.1 | MIT OR Apache-2.0 | crates.io |
| `rand_core` | 0.6.4 | MIT OR Apache-2.0 | crates.io |
| `rand_pcg` | 0.10.2 | MIT OR Apache-2.0 | crates.io |
| `raw-window-handle` | 0.6.2 | MIT OR Apache-2.0 OR Zlib | crates.io |
| `read-fonts` | 0.41.0 | MIT OR Apache-2.0 | crates.io |
| `ref-cast` | 1.0.26 | MIT OR Apache-2.0 | crates.io |
| `ref-cast-impl` | 1.0.26 | MIT OR Apache-2.0 | crates.io |
| `regex` | 1.13.1 | MIT OR Apache-2.0 | crates.io |
| `regex-automata` | 0.4.18 | MIT OR Apache-2.0 | crates.io |
| `regex-syntax` | 0.8.11 | MIT OR Apache-2.0 | crates.io |
| `reqwest` | 0.13.4 | MIT OR Apache-2.0 | crates.io |
| `resolv-conf` | 0.7.6 | MIT OR Apache-2.0 | crates.io |
| `resvg` | 0.48.1 | Apache-2.0 OR MIT | crates.io |
| `rgb` | 0.8.53 | MIT | crates.io |
| `ring` | 0.17.14 | Apache-2.0 AND ISC | crates.io |
| `roxmltree` | 0.20.0 | MIT OR Apache-2.0 | crates.io |
| `roxmltree` | 0.21.1 | MIT OR Apache-2.0 | crates.io |
| `rsa` | 0.10.0-rc.18 | MIT OR Apache-2.0 | crates.io |
| `rsa_public_encrypt_pkcs1` | 0.4.0 | ISC | crates.io |
| `rustc-hash` | 2.1.3 | Apache-2.0 OR MIT | crates.io |
| `rustc_version` | 0.4.1 | MIT OR Apache-2.0 | crates.io |
| `rustix` | 1.1.4 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | crates.io |
| `rustls` | 0.23.43 | Apache-2.0 OR ISC OR MIT | crates.io |
| `rustls-native-certs` | 0.8.4 | Apache-2.0 OR ISC OR MIT | crates.io |
| `rustls-pki-types` | 1.15.1 | MIT OR Apache-2.0 | crates.io |
| `rustls-platform-verifier` | 0.7.0 | MIT OR Apache-2.0 | crates.io |
| `rustls-webpki` | 0.103.14 | ISC | crates.io |
| `rustversion` | 1.0.23 | MIT OR Apache-2.0 | crates.io |
| `ryu` | 1.0.23 | Apache-2.0 OR BSL-1.0 | crates.io |
| `schemars` | 1.2.2 | MIT | crates.io |
| `schemars_derive` | 1.2.2 | MIT | crates.io |
| `scopeguard` | 1.2.0 | MIT OR Apache-2.0 | crates.io |
| `semver` | 1.0.28 | MIT OR Apache-2.0 | crates.io |
| `serde` | 1.0.229 | MIT OR Apache-2.0 | crates.io |
| `serde_core` | 1.0.229 | MIT OR Apache-2.0 | crates.io |
| `serde_derive` | 1.0.229 | MIT OR Apache-2.0 | crates.io |
| `serde_derive_internals` | 0.30.0 | MIT OR Apache-2.0 | crates.io |
| `serde_json` | 1.0.151 | MIT OR Apache-2.0 | crates.io |
| `serde_spanned` | 1.1.1 | MIT OR Apache-2.0 | crates.io |
| `serde_urlencoded` | 0.7.1 | MIT/Apache-2.0 | crates.io |
| `serdect` | 0.4.3 | Apache-2.0 OR MIT | crates.io |
| `sha1` | 0.11.0 | MIT OR Apache-2.0 | crates.io |
| `sha2` | 0.10.9 | MIT OR Apache-2.0 | crates.io |
| `sha2` | 0.11.0 | MIT OR Apache-2.0 | crates.io |
| `sharded-slab` | 0.1.7 | MIT | crates.io |
| `shlex` | 2.0.1 | MIT OR Apache-2.0 | crates.io |
| `signal-hook-registry` | 1.4.8 | MIT OR Apache-2.0 | crates.io |
| `signature` | 3.0.0 | Apache-2.0 OR MIT | crates.io |
| `simd-adler32` | 0.3.10 | MIT | crates.io |
| `simd_cesu8` | 1.2.0 | Apache-2.0 OR MIT | crates.io |
| `simdnbt` | 0.10.0 | MIT | crates.io |
| `simdnbt-derive` | 0.10.0 | MIT | crates.io |
| `simdutf8` | 0.1.5 | MIT OR Apache-2.0 | crates.io |
| `similar` | 2.7.0 | Apache-2.0 | crates.io |
| `simple_asn1` | 0.5.4 | ISC | crates.io |
| `simplecss` | 0.2.2 | Apache-2.0 OR MIT | crates.io |
| `siphasher` | 1.0.3 | MIT/Apache-2.0 | crates.io |
| `skrifa` | 0.44.0 | MIT OR Apache-2.0 | crates.io |
| `slab` | 0.4.12 | MIT | crates.io |
| `slotmap` | 1.1.1 | Zlib | crates.io |
| `smallvec` | 1.15.2 | MIT OR Apache-2.0 | crates.io |
| `smol_str` | 0.2.2 | MIT OR Apache-2.0 | crates.io |
| `socket2` | 0.6.5 | MIT OR Apache-2.0 | crates.io |
| `socks5-impl` | 0.9.6 | GPL-3.0-or-later | crates.io |
| `spin` | 0.10.1 | MIT | crates.io |
| `spki` | 0.8.0 | Apache-2.0 OR MIT | crates.io |
| `stable_deref_trait` | 1.2.1 | MIT OR Apache-2.0 | crates.io |
| `static_assertions` | 1.1.0 | MIT OR Apache-2.0 | crates.io |
| `strict-num` | 0.1.1 | MIT | crates.io |
| `strsim` | 0.11.1 | MIT | crates.io |
| `subtle` | 2.6.1 | BSD-3-Clause | crates.io |
| `svgtypes` | 0.16.1 | Apache-2.0 OR MIT | crates.io |
| `syn` | 2.0.119 | MIT OR Apache-2.0 | crates.io |
| `syn` | 3.0.3 | MIT OR Apache-2.0 | crates.io |
| `sync_wrapper` | 1.0.2 | Apache-2.0 | crates.io |
| `synstructure` | 0.13.2 | MIT | crates.io |
| `tagptr` | 0.2.0 | MIT/Apache-2.0 | crates.io |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | crates.io |
| `thiserror` | 1.0.69 | MIT OR Apache-2.0 | crates.io |
| `thiserror` | 2.0.20 | MIT OR Apache-2.0 | crates.io |
| `thiserror-impl` | 1.0.69 | MIT OR Apache-2.0 | crates.io |
| `thiserror-impl` | 2.0.20 | MIT OR Apache-2.0 | crates.io |
| `thread_local` | 1.1.10 | MIT OR Apache-2.0 | crates.io |
| `tiny-skia` | 0.12.0 | BSD-3-Clause | crates.io |
| `tiny-skia-path` | 0.12.0 | BSD-3-Clause | crates.io |
| `tinystr` | 0.8.3 | Unicode-3.0 | crates.io |
| `tinyvec` | 1.12.0 | Zlib OR Apache-2.0 OR MIT | crates.io |
| `tinyvec_macros` | 0.1.1 | MIT OR Apache-2.0 OR Zlib | crates.io |
| `tokio` | 1.53.1 | MIT | crates.io |
| `tokio-macros` | 2.7.2 | MIT | crates.io |
| `tokio-rustls` | 0.26.4 | MIT OR Apache-2.0 | crates.io |
| `tokio-util` | 0.7.19 | MIT | crates.io |
| `toml` | 0.9.12+spec-1.1.0 | MIT OR Apache-2.0 | crates.io |
| `toml_datetime` | 0.7.5+spec-1.1.0 | MIT OR Apache-2.0 | crates.io |
| `toml_datetime` | 1.1.1+spec-1.1.0 | MIT OR Apache-2.0 | crates.io |
| `toml_edit` | 0.25.13+spec-1.1.0 | MIT OR Apache-2.0 | crates.io |
| `toml_parser` | 1.1.3+spec-1.1.0 | MIT OR Apache-2.0 | crates.io |
| `toml_writer` | 1.1.2+spec-1.1.0 | MIT OR Apache-2.0 | crates.io |
| `tower` | 0.5.3 | MIT | crates.io |
| `tower-http` | 0.6.11 | MIT | crates.io |
| `tower-layer` | 0.3.3 | MIT | crates.io |
| `tower-service` | 0.3.3 | MIT | crates.io |
| `tracing` | 0.1.44 | MIT | crates.io |
| `tracing-attributes` | 0.1.31 | MIT | crates.io |
| `tracing-core` | 0.1.36 | MIT | crates.io |
| `tracing-log` | 0.2.0 | MIT | crates.io |
| `tracing-subscriber` | 0.3.23 | MIT | crates.io |
| `try-lock` | 0.2.5 | MIT | crates.io |
| `typeid` | 1.0.3 | MIT OR Apache-2.0 | crates.io |
| `typenum` | 1.20.1 | MIT OR Apache-2.0 | crates.io |
| `unicode-bidi` | 0.3.18 | MIT OR Apache-2.0 | crates.io |
| `unicode-ident` | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 | crates.io |
| `unicode-script` | 0.5.8 | MIT OR Apache-2.0 | crates.io |
| `unicode-segmentation` | 1.13.3 | MIT OR Apache-2.0 | crates.io |
| `unicode-vo` | 0.1.0 | MIT/Apache-2.0 | crates.io |
| `unicode-xid` | 0.2.6 | MIT OR Apache-2.0 | crates.io |
| `untrusted` | 0.9.0 | ISC | crates.io |
| `url` | 2.5.8 | MIT OR Apache-2.0 | crates.io |
| `usvg` | 0.48.1 | Apache-2.0 OR MIT | crates.io |
| `utf8_iter` | 1.0.4 | Apache-2.0 OR MIT | crates.io |
| `utf8parse` | 0.2.2 | Apache-2.0 OR MIT | crates.io |
| `uuid` | 1.24.0 | Apache-2.0 OR MIT | crates.io |
| `variadics_please` | 1.1.0 | MIT OR Apache-2.0 | crates.io |
| `version_check` | 0.9.5 | MIT/Apache-2.0 | crates.io |
| `want` | 0.3.1 | MIT | crates.io |
| `wgpu-types` | 29.0.4 | MIT OR Apache-2.0 | crates.io |
| `winnow` | 0.7.15 | MIT | crates.io |
| `winnow` | 1.0.4 | MIT | crates.io |
| `writeable` | 0.6.3 | Unicode-3.0 | crates.io |
| `xmlwriter` | 0.1.0 | MIT | crates.io |
| `yoke` | 0.8.3 | Unicode-3.0 | crates.io |
| `yoke-derive` | 0.8.2 | Unicode-3.0 | crates.io |
| `zerocopy` | 0.8.56 | BSD-2-Clause OR Apache-2.0 OR MIT | crates.io |
| `zerofrom` | 0.1.8 | Unicode-3.0 | crates.io |
| `zerofrom-derive` | 0.1.7 | Unicode-3.0 | crates.io |
| `zeroize` | 1.9.0 | Apache-2.0 OR MIT | crates.io |
| `zerotrie` | 0.2.4 | Unicode-3.0 | crates.io |
| `zerovec` | 0.11.6 | Unicode-3.0 | crates.io |
| `zerovec-derive` | 0.11.3 | Unicode-3.0 | crates.io |
| `zlib-rs` | 0.6.7 | Zlib | crates.io |
| `zmij` | 1.0.23 | MIT | crates.io |
