// Copyright 2026 malakai
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::env;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

const GODOT_REPO: &str = "https://github.com/godotengine/godot.git";

/// libgodot.h first shipped in this release.
const MIN_VERSION: (u32, u32) = (4, 6);

/// Version features, in Cargo.toml order, with the selector each maps to.
const VERSION_FEATURES: &[(&str, &str)] = &[
    ("GODOT_LATEST", "latest"),
    ("GODOT_STABLE", "stable"),
    ("GODOT_4_6", "4.6"),
    ("GODOT_4_7", "4.7"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LinkMode {
    /// Pull libgodot.a into the binary at build time.
    Static,
    /// Link against libgodot.so/.dylib/.dll at build time.
    Dynamic,
    /// Emit no link directives; the host binary provides the symbols
    /// (or resolves them at run time with dlopen).
    None,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for var in [
        "GODOT_SOURCE_PATH",
        "GODOT_LIB_DIR",
        "GODOT_LIB_NAME",
        "GODOT_LINK",
        "GODOT_TARGET",
        "GODOT_WHOLE_ARCHIVE",
        "GODOT_EXTRA_LIBS",
        "GODOT_RPATH",
        "GODOT_VERSION",
        "GODOT_CLONE_DIR",
        "GODOT_UPDATE_BINDINGS",
        "CARGO_NET_OFFLINE",
        "DOCS_RS",
        "PYTHON",
        "SCONS",
        "SCONSFLAGS",
        "AR",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }

    let godot_path = locate_source();
    generate_bindings(godot_path.as_deref());
    emit_link_flags(godot_path.as_deref());
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"))
}

fn out_dir() -> PathBuf {
    PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"))
}

fn is_docs_rs() -> bool {
    env::var_os("DOCS_RS").is_some()
}

fn is_offline() -> bool {
    env::var("CARGO_NET_OFFLINE").is_ok_and(|v| v == "true" || v == "1")
}

/// Takes an exclusive lock, held until the returned file is dropped, so that
/// concurrent builds (say rust-analyzer next to a terminal) don't clone or
/// run scons into the same directory at once.
fn lock(path: &Path) -> File {
    let file = File::create(path).unwrap_or_else(|e| panic!("unable to create {}: {e}", path.display()));
    if file.try_lock().is_err() {
        println!("cargo:warning=waiting for another build holding {}", path.display());
        file.lock().unwrap_or_else(|e| panic!("unable to lock {}: {e}", path.display()));
    }
    file
}

// ----------------------------------------------------------------- source

fn has_godot_source(path: &Path) -> bool {
    path.join("core/version.h").exists()
}

/// GODOT_SOURCE_PATH, else ./godot, else a clone. None means there is no
/// source to be had (docs.rs, or offline without a cached clone), and the
/// checked-in bindings stand in.
fn locate_source() -> Option<PathBuf> {
    if let Ok(path) = env::var("GODOT_SOURCE_PATH") {
        let path = PathBuf::from(path);
        if !has_godot_source(&path) {
            panic!("GODOT_SOURCE_PATH={} is not a Godot source tree (no core/version.h)", path.display());
        }
        return Some(path);
    }

    let local = manifest_dir().join("godot");
    if has_godot_source(&local) {
        return Some(local);
    }

    // docs.rs has no network and only needs the API.
    if is_docs_rs() {
        return None;
    }
    clone_godot()
}

/// Shallow-clones the selected Godot version and returns its path. Clones are
/// cached per ref under GODOT_CLONE_DIR (default `<target>/godot-src`), so
/// this only hits the network once per version; delete the directory to
/// refresh `latest`.
fn clone_godot() -> Option<PathBuf> {
    let selector = version_selector();
    let clone_root = match env::var("GODOT_CLONE_DIR") {
        Ok(dir) => PathBuf::from(dir),
        Err(_) => target_dir().join("godot-src"),
    };

    let git_ref = if is_offline() {
        match cached_ref(&clone_root, &selector) {
            Some(git_ref) => git_ref,
            None => {
                println!(
                    "cargo:warning=offline and no cached Godot clone matches {selector:?}; \
                     using the checked-in bindings. Set GODOT_SOURCE_PATH to use a local tree."
                );
                return None;
            }
        }
    } else {
        resolve_ref(&selector).or_else(|| cached_ref(&clone_root, &selector)).unwrap_or_else(|| {
            panic!("unable to resolve Godot {selector:?}: git ls-remote failed and nothing matching is cached")
        })
    };

    let dest = clone_root.join(&git_ref);
    if has_godot_source(&dest) {
        return Some(dest);
    }
    if is_offline() {
        // cached_ref only returns populated clones, so this is a pass-through
        // ref (branch or tag name) that has never been cloned.
        println!("cargo:warning=offline and Godot {git_ref} isn't cloned yet; using the checked-in bindings");
        return None;
    }

    fs::create_dir_all(&clone_root).expect("unable to create Godot clone directory");
    let _lock = lock(&clone_root.join(format!(".{git_ref}.lock")));
    // Another build may have finished the clone while we waited.
    if has_godot_source(&dest) {
        return Some(dest);
    }

    println!("cargo:warning=no Godot source found; shallow-cloning {git_ref} into {}", dest.display());

    // Clone next to the destination and rename, so an interrupted build never
    // leaves a half-populated tree that later passes has_godot_source.
    let tmp = clone_root.join(format!(".{git_ref}.partial"));
    let _ = fs::remove_dir_all(&tmp);
    let status = Command::new("git")
        .args(["clone", "--depth", "1", "--single-branch", "--branch", &git_ref, GODOT_REPO])
        .arg(&tmp)
        .stdout(Stdio::from(io::stderr()))
        .status()
        .unwrap_or_else(|e| panic!("failed to run git (is it installed?): {e}"));
    if !status.success() {
        let _ = fs::remove_dir_all(&tmp);
        panic!("git clone of Godot {git_ref} failed ({status})");
    }
    let _ = fs::remove_dir_all(&dest);
    fs::rename(&tmp, &dest).expect("unable to move Godot clone into place");
    Some(dest)
}

/// GODOT_VERSION wins over the features; with neither, `stable`.
fn version_selector() -> String {
    if let Ok(version) = env::var("GODOT_VERSION") {
        return version;
    }

    let enabled: Vec<&str> = VERSION_FEATURES
        .iter()
        .filter(|(feature, _)| env::var_os(format!("CARGO_FEATURE_{feature}")).is_some())
        .map(|&(_, selector)| selector)
        .collect();
    match enabled.as_slice() {
        [] => "stable".to_string(),
        [one] => one.to_string(),
        many => panic!(
            "multiple Godot version features enabled ({many:?}); enable only one, \
             or set GODOT_VERSION to pick"
        ),
    }
}

/// What a selector asks for.
enum Wanted {
    /// A branch or tag name, used as is.
    Ref(String),
    /// The newest stable tag, optionally within one major.minor.
    Stable(Option<(u32, u32)>),
}

fn parse_selector(selector: &str) -> Wanted {
    match selector {
        "latest" | "master" => Wanted::Ref("master".to_string()),
        "stable" => Wanted::Stable(None),
        s => match s.split_once('.').map(|(a, b)| (a.parse(), b.parse())) {
            Some((Ok(major), Ok(minor))) => {
                check_min_version((major, minor), s);
                Wanted::Stable(Some((major, minor)))
            }
            _ => {
                if let Some(version) = parse_stable_tag(s) {
                    check_min_version((version.0, version.1), s);
                }
                Wanted::Ref(s.to_string())
            }
        },
    }
}

fn check_min_version(version: (u32, u32), what: &str) {
    if version < MIN_VERSION {
        panic!(
            "Godot {what} has no libgodot; it first shipped in {}.{}",
            MIN_VERSION.0, MIN_VERSION.1
        );
    }
}

/// `4.6.2-stable` -> (4, 6, 2); `4.6-stable` -> (4, 6, 0).
fn parse_stable_tag(tag: &str) -> Option<(u32, u32, u32)> {
    let version = tag.strip_suffix("-stable")?;
    let mut parts = version.split('.').map(str::parse::<u32>);
    let major = parts.next()?.ok()?;
    let minor = parts.next()?.ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The newest tag among `tags` that satisfies `wanted`.
fn best_stable<'a>(tags: impl Iterator<Item = &'a str>, wanted: Option<(u32, u32)>) -> Option<String> {
    tags.filter_map(|tag| {
        let version = parse_stable_tag(tag)?;
        (version >= (MIN_VERSION.0, MIN_VERSION.1, 0) && wanted.is_none_or(|w| w == (version.0, version.1)))
            .then(|| (version, tag.to_string()))
    })
    .max()
    .map(|(_, tag)| tag)
}

/// Turns a selector into a branch or tag name, asking the remote for the
/// newest matching tag. None if the remote can't be reached.
fn resolve_ref(selector: &str) -> Option<String> {
    let wanted = match parse_selector(selector) {
        Wanted::Ref(git_ref) => return Some(git_ref),
        Wanted::Stable(wanted) => wanted,
    };

    let output = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", GODOT_REPO])
        .output()
        .unwrap_or_else(|e| panic!("failed to run git (is it installed?): {e}"));
    if !output.status.success() {
        println!("cargo:warning=git ls-remote {GODOT_REPO} failed ({})", output.status);
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let tags = stdout.lines().filter_map(|line| line.split_once("refs/tags/").map(|(_, tag)| tag));
    Some(best_stable(tags, wanted).unwrap_or_else(|| panic!("no Godot stable release matches {selector:?}")))
}

/// The best already-cloned ref for a selector, for when the remote is off
/// limits.
fn cached_ref(clone_root: &Path, selector: &str) -> Option<String> {
    let cached: Vec<String> = fs::read_dir(clone_root)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| has_godot_source(&entry.path()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();

    match parse_selector(selector) {
        Wanted::Ref(git_ref) => cached.contains(&git_ref).then_some(git_ref),
        Wanted::Stable(wanted) => best_stable(cached.iter().map(String::as_str), wanted),
    }
}

/// OUT_DIR is `<target>[/<triple>]/<profile>/build/<pkg>-<hash>/out`.
fn target_dir() -> PathBuf {
    let out_dir = out_dir();
    out_dir
        .ancestors()
        .nth(4)
        .map(Path::to_path_buf)
        .unwrap_or(out_dir)
}

// ---------------------------------------------------------------- bindings

/// Writes `$OUT_DIR/bindings.rs`, which src/lib.rs includes. With no engine
/// source, the checked-in src/bindings.rs is used instead.
fn generate_bindings(godot_path: Option<&Path>) {
    let out = out_dir().join("bindings.rs");
    let checked_in = manifest_dir().join("src/bindings.rs");

    let Some(godot_path) = godot_path else {
        if !is_docs_rs() {
            println!(
                "cargo:warning=no Godot source available; using the checked-in src/bindings.rs, \
                 which may not match the library you link against"
            );
        }
        println!("cargo:rerun-if-changed={}", checked_in.display());
        fs::copy(&checked_in, &out).expect("unable to copy src/bindings.rs");
        return;
    };

    let version_h = godot_path.join("core/version.h");
    let libgodot_h = godot_path.join("core/extension/libgodot.h");
    if !libgodot_h.exists() {
        panic!(
            "{} has no core/extension/libgodot.h; libgodot needs Godot {}.{} or newer",
            godot_path.display(),
            MIN_VERSION.0,
            MIN_VERSION.1
        );
    }

    println!("cargo:rerun-if-changed={}", version_h.display());
    println!("cargo:rerun-if-changed={}", libgodot_h.display());

    // Always on the include path; it only holds what the tree is missing.
    let gen_dir = generate_missing_headers(godot_path);
    let bindings = bindgen::Builder::default()
        .header(version_h.display().to_string())
        .header(libgodot_h.display().to_string())
        .clang_arg(format!("-I{}", godot_path.display()))
        .clang_arg(format!("-I{}", gen_dir.display()))
        // 4.6's libgodot.h includes the interface header without its directory.
        .clang_arg(format!("-I{}", gen_dir.join("core/extension").display()))
        .generate()
        .expect("Unable to generate bindings");

    bindings.write_to_file(&out).expect("Unable to write bindings");

    // Refreshing the fallback copy is a deliberate act: build scripts aren't
    // supposed to touch the source tree (cargo publish rejects it).
    if env::var("GODOT_UPDATE_BINDINGS").is_ok_and(|v| v != "0") {
        fs::copy(&out, &checked_in).expect("unable to update src/bindings.rs");
        println!("cargo:warning=updated {}", checked_in.display());
    }
}

/// The headers we bind include files that scons writes on the first engine
/// build, so a fresh clone doesn't have them yet. Produce the missing ones in
/// OUT_DIR, leaving the source tree as scons expects it.
fn generate_missing_headers(godot_path: &Path) -> PathBuf {
    let gen_dir = out_dir().join("gen");
    let _ = fs::remove_dir_all(&gen_dir);
    fs::create_dir_all(gen_dir.join("core/extension")).expect("unable to create generated header dir");

    if !godot_path.join("core/version_generated.gen.h").exists() {
        write_version_header(godot_path, &gen_dir);
    }
    // Generated from JSON since 4.6; older trees ship a plain header.
    if godot_path.join("core/extension/gdextension_interface.json").exists()
        && !godot_path.join("core/extension/gdextension_interface.gen.h").exists()
    {
        write_interface_header(godot_path, &gen_dir);
    }
    gen_dir
}

/// Mirrors what scons writes from version.py.
fn write_version_header(godot_path: &Path, gen_dir: &Path) {
    let version_py = godot_path.join("version.py");
    println!("cargo:rerun-if-changed={}", version_py.display());
    let source = fs::read_to_string(&version_py).expect("unable to read Godot's version.py");
    let field = |key: &str| {
        source
            .lines()
            .filter_map(|line| line.split_once('='))
            .find(|(k, _)| k.trim() == key)
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
            .unwrap_or_else(|| panic!("version.py has no {key}"))
    };

    let docs = field("docs");
    let header = format!(
        "#pragma once\n\
         #define GODOT_VERSION_SHORT_NAME \"{}\"\n\
         #define GODOT_VERSION_NAME \"{}\"\n\
         #define GODOT_VERSION_MAJOR {}\n\
         #define GODOT_VERSION_MINOR {}\n\
         #define GODOT_VERSION_PATCH {}\n\
         #define GODOT_VERSION_STATUS \"{}\"\n\
         #define GODOT_VERSION_BUILD \"custom_build\"\n\
         #define GODOT_VERSION_MODULE_CONFIG \"{}\"\n\
         #define GODOT_VERSION_WEBSITE \"{}\"\n\
         #define GODOT_VERSION_DOCS_BRANCH \"{docs}\"\n\
         #define GODOT_VERSION_DOCS_URL \"https://docs.godotengine.org/en/\" GODOT_VERSION_DOCS_BRANCH\n",
        field("short_name"),
        field("name"),
        field("major"),
        field("minor"),
        field("patch"),
        field("status"),
        field("module_config"),
        field("website"),
    );

    fs::write(gen_dir.join("core/version_generated.gen.h"), header)
        .expect("unable to write version_generated.gen.h");
}

/// Too much logic to mirror, so run Godot's own generator, the same one scons
/// calls. Needs Python, which building the engine needs anyway.
fn write_interface_header(godot_path: &Path, gen_dir: &Path) {
    let json = godot_path.join("core/extension/gdextension_interface.json");
    println!("cargo:rerun-if-changed={}", json.display());
    let out = gen_dir.join("core/extension/gdextension_interface.gen.h");

    let python = env::var("PYTHON").unwrap_or_else(|_| "python3".to_string());
    let script = "import sys\n\
                  sys.path[:0] = ['.', 'core/extension']\n\
                  import make_interface_header\n\
                  make_interface_header.run([sys.argv[1]], ['core/extension/gdextension_interface.json'], None)\n";
    let status = Command::new(&python)
        .current_dir(godot_path)
        .args(["-c", script])
        .arg(&out)
        .status()
        .unwrap_or_else(|e| panic!("failed to run {python} (set PYTHON to override): {e}"));
    if !status.success() {
        panic!("generating gdextension_interface.gen.h failed ({status})");
    }
}

// linking

fn emit_link_flags(godot_path: Option<&Path>) {
    let mode = link_mode();

    // So the crate can report what it was actually built with, including when
    // GODOT_LINK overrode the features.
    println!(
        "cargo:rustc-env=GODOT_LINKAGE={}",
        match mode {
            LinkMode::Static => "static",
            LinkMode::Dynamic => "dynamic",
            LinkMode::None => "none",
        }
    );

    if mode == LinkMode::None {
        return;
    }

    let custom_lib_dir = env::var("GODOT_LIB_DIR").ok().map(PathBuf::from);
    let lib_dir = match (&custom_lib_dir, godot_path) {
        (Some(dir), _) => dir.clone(),
        (None, Some(path)) => path.join("bin"),
        (None, None) => panic!(
            "no Godot source to find a library in; set GODOT_LIB_DIR to a directory \
             holding a prebuilt Godot library, or GODOT_LINK=none"
        ),
    };

    let lib_name = match env::var("GODOT_LIB_NAME") {
        Ok(name) => name,
        Err(_) => pick_lib(&lib_dir, mode, godot_path.filter(|_| custom_lib_dir.is_none())),
    };

    let (lib_dir, lib_name) = match mode {
        LinkMode::Static => merge_static_parts(&lib_dir, lib_name, godot_path),
        _ => (lib_dir, lib_name),
    };
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    match mode {
        LinkMode::Static => {
            // Godot registers most things through explicit calls, so on-demand
            // archive member extraction is normally enough. Set
            // GODOT_WHOLE_ARCHIVE=1 if a build turns out to rely on static
            // initializers that the linker would otherwise drop.
            if env::var("GODOT_WHOLE_ARCHIVE").is_ok_and(|v| v != "0") {
                println!("cargo:rustc-link-lib=static:+whole-archive={lib_name}");
            } else {
                println!("cargo:rustc-link-lib=static={lib_name}");
            }
            link_static_dependencies();
        }
        LinkMode::Dynamic => {
            println!("cargo:rustc-link-lib=dylib={lib_name}");
            // Convenience for this crate's own examples/tests: Cargo only
            // applies link args to the package that emits them, so a downstream
            // binary still has to set its own rpath (or LD_LIBRARY_PATH).
            if env::var("GODOT_RPATH").is_ok_and(|v| v != "0") {
                println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
            }
        }
        LinkMode::None => unreachable!(),
    }

    // Anything the local engine build needs on top, e.g.
    // GODOT_EXTRA_LIBS="dylib=X11,static=embree3".
    if let Ok(extra) = env::var("GODOT_EXTRA_LIBS") {
        for lib in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}

/// Finds the library for this build, running scons first when the
/// `build-godot` feature is on and only a library for some other
/// configuration exists. `build_in` is the source tree scons may build in;
/// None when the library directory isn't ours to write to.
fn pick_lib(lib_dir: &Path, mode: LinkMode, build_in: Option<&Path>) -> String {
    let target = godot_target();
    let build_godot_on = env::var_os("CARGO_FEATURE_BUILD_GODOT").is_some();

    let mut found = find_lib(lib_dir, mode, &target);
    if !matches!(found, Some((Match::Exact, _)))
        && build_godot_on
        && let Some(godot_path) = build_in
    {
        fs::create_dir_all(lib_dir).expect("unable to create Godot bin directory");
        let _lock = lock(&lib_dir.join(".libgodot-rs-build.lock"));
        // Another build may have produced it while we waited.
        found = find_lib(lib_dir, mode, &target);
        if !matches!(found, Some((Match::Exact, _))) {
            build_godot(godot_path, mode, &target);
            found = find_lib(lib_dir, mode, &target);
        }
    }

    match found {
        Some((Match::Exact, name)) => name,
        Some((_, name)) => {
            println!(
                "cargo:warning=no {target} Godot library for this target in {}; using {name}. \
                 Enable `build-godot` to build one, or set GODOT_LIB_NAME / GODOT_TARGET.",
                lib_dir.display()
            );
            name
        }
        None => panic!(
            "no {} Godot library found in {}.\n\
             Build one with `scons platform=<p> target={target} library_type={} ...` (or enable \
             the `build-godot` feature), then point GODOT_LIB_DIR at the output directory \
             (or set GODOT_LIB_NAME explicitly).",
            match mode {
                LinkMode::Static => "static",
                _ => "shared",
            },
            lib_dir.display(),
            scons_library_type(mode),
        ),
    }
}

/// A `static_library` build leaves most of the engine outside the named
/// archive: core, main, servers, scene, drivers and every module each get
/// their own, under bin/obj/ (or next to their sources with
/// `redirect_build_objects=no`), and they reference each other in cycles.
/// Rather than make every downstream binary pass --start-group, fold them all
/// into one archive in OUT_DIR, which the linker can resolve in any order.
/// Returns the directory and name to link, unchanged if there are no parts.
fn merge_static_parts(lib_dir: &Path, lib_name: String, godot_path: Option<&Path>) -> (PathBuf, String) {
    let windows_msvc = env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|e| e == "msvc");
    let (prefix, ext) = if windows_msvc { ("", "lib") } else { ("lib", "a") };
    let main = lib_dir.join(format!("{prefix}{lib_name}.{ext}"));
    // `godot.linuxbsd.template_debug.x86_64` -> `.linuxbsd.template_debug.x86_64.a`
    let Some(config) = lib_name.strip_prefix("godot") else {
        return (lib_dir.to_path_buf(), lib_name);
    };
    let part_suffix = format!("{config}.{ext}");

    let mut parts = Vec::new();
    collect_archives(&lib_dir.join("obj"), &part_suffix, Path::new(""), &mut parts);
    if parts.is_empty()
        && let Some(godot_path) = godot_path
    {
        // bin/ only holds final outputs once obj/ isn't in use.
        collect_archives(godot_path, &part_suffix, &godot_path.join("bin"), &mut parts);
    }
    parts.retain(|p| *p != main);
    if parts.is_empty() {
        return (lib_dir.to_path_buf(), lib_name);
    }
    parts.sort();
    parts.insert(0, main);

    let merged_name = "godot_merged";
    let out = out_dir().join(format!("{prefix}{merged_name}.{ext}"));
    let manifest = out_dir().join("godot_merged.inputs");
    let listing = parts.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n");
    for part in &parts {
        println!("cargo:rerun-if-changed={}", part.display());
    }

    let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    let newest_input = parts.iter().filter_map(|p| mtime(p)).max();
    let up_to_date = fs::read_to_string(&manifest).is_ok_and(|m| m == listing)
        && mtime(&out).is_some_and(|out_time| newest_input.is_some_and(|input| out_time >= input));
    if !up_to_date {
        let _ = fs::remove_file(&out);
        merge_archives(&out, &parts);
        fs::write(&manifest, &listing).expect("unable to write merged archive manifest");
    }
    (out_dir(), merged_name.to_string())
}

/// Recursively collects files ending in `suffix`, skipping VCS metadata and
/// `skip`.
fn collect_archives(dir: &Path, suffix: &str, skip: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            if name != ".git" && path != skip {
                collect_archives(&path, suffix, skip, found);
            }
        } else if name.ends_with(suffix) {
            found.push(path);
        }
    }
}

/// Combines static archives into one with the platform's archiver.
fn merge_archives(out: &Path, inputs: &[PathBuf]) {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    println!("cargo:warning=merging {} Godot archives into {}", inputs.len(), out.display());

    let status = if target_os == "macos" || target_os == "ios" {
        Command::new("libtool").args(["-static", "-o"]).arg(out).args(inputs).status()
    } else if target_env == "msvc" {
        Command::new("lib.exe").arg("/NOLOGO").arg(format!("/OUT:{}", out.display())).args(inputs).status()
    } else {
        // GNU ar's MRI mode is the one way to splice whole archives together.
        use std::io::Write;
        let ar = env::var("AR").unwrap_or_else(|_| "ar".to_string());
        let mut script = format!("CREATE {}\n", out.display());
        for input in inputs {
            script += &format!("ADDLIB {}\n", input.display());
        }
        script += "SAVE\nEND\n";
        Command::new(&ar).arg("-M").stdin(Stdio::piped()).spawn().and_then(|mut child| {
            child.stdin.take().expect("piped stdin").write_all(script.as_bytes())?;
            child.wait()
        })
    };

    match status {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("merging Godot's static archives failed ({status})"),
        Err(e) => panic!("unable to run the archiver to merge Godot's static archives: {e}"),
    }
}

fn scons_library_type(mode: LinkMode) -> &'static str {
    match mode {
        LinkMode::Static => "static_library",
        _ => "shared_library",
    }
}

/// Space-separated `key=value` flags from SCONSFLAGS.
fn scons_flag(key: &str) -> Option<String> {
    env::var("SCONSFLAGS")
        .ok()?
        .split_whitespace()
        .filter_map(|flag| flag.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

/// editor, template_debug or template_release: GODOT_TARGET, else whatever
/// SCONSFLAGS builds, else matching the Cargo profile.
fn godot_target() -> String {
    if let Ok(target) = env::var("GODOT_TARGET") {
        return target;
    }
    if let Some(target) = scons_flag("target") {
        return target;
    }
    // Cargo's PROFILE is only ever "debug" or "release".
    match env::var("PROFILE").as_deref() {
        Ok("release") => "template_release",
        _ => "template_debug",
    }
    .to_string()
}

/// Godot's name for the Cargo target's platform.
fn godot_platform() -> Option<&'static str> {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    Some(match target_os.as_str() {
        "linux" | "freebsd" | "netbsd" | "openbsd" => "linuxbsd",
        "macos" => "macos",
        "ios" => "ios",
        "windows" => "windows",
        "android" => "android",
        "emscripten" => "web",
        _ => return None,
    })
}

/// Godot's name for the Cargo target's architecture.
fn godot_arch() -> Option<&'static str> {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    Some(match target_arch.as_str() {
        "x86_64" => "x86_64",
        "x86" => "x86_32",
        "aarch64" => "arm64",
        "arm" => "arm32",
        "riscv64" => "rv64",
        "powerpc64" => "ppc64",
        "loongarch64" => "loongarch64",
        "wasm32" => "wasm32",
        _ => return None,
    })
}

/// Runs scons in the source tree for the current Cargo target. SCONSFLAGS is
/// honoured by scons itself; any option set there is left out of the command
/// line, since command-line arguments would override it.
fn build_godot(godot_path: &Path, mode: LinkMode, target: &str) {
    let user_flags = env::var("SCONSFLAGS").unwrap_or_default();

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let platform = godot_platform().unwrap_or_else(|| {
        panic!("build-godot: no Godot platform known for target_os={target_os}; build it manually")
    });
    let arch = godot_arch().unwrap_or_else(|| {
        panic!("build-godot: no Godot arch known for target_arch={target_arch}; build it manually")
    });

    let mut args = Vec::new();
    for (key, value) in [
        ("platform", platform),
        ("arch", arch),
        ("target", target),
        ("library_type", scons_library_type(mode)),
    ] {
        if scons_flag(key).is_none() {
            args.push(format!("{key}={value}"));
        }
    }
    if !user_flags.split_whitespace().any(|flag| flag.starts_with("-j"))
        && let Ok(jobs) = env::var("NUM_JOBS")
    {
        args.push(format!("-j{jobs}"));
    }

    let scons = env::var("SCONS").unwrap_or_else(|_| "scons".to_string());
    println!(
        "cargo:warning=building Godot with `{scons} {}`{}; this takes a while (cargo build -vv shows progress)",
        args.join(" "),
        if user_flags.is_empty() { String::new() } else { format!(" and SCONSFLAGS={user_flags:?}") },
    );

    // Build-script stdout is parsed for cargo: directives, so keep scons'
    // chatter on stderr.
    let status = Command::new(&scons)
        .current_dir(godot_path)
        .args(&args)
        .stdout(Stdio::from(io::stderr()))
        .status()
        .unwrap_or_else(|e| panic!("failed to run {scons} (set SCONS to override): {e}"));
    if !status.success() {
        panic!("scons failed ({status}); rerun with cargo build -vv to see its output");
    }
}

fn link_mode() -> LinkMode {
    // Nothing gets linked when documenting, and docs.rs has no engine anyway.
    if is_docs_rs() {
        return LinkMode::None;
    }

    // Env var wins, so a single checkout can build both ways without touching
    // feature flags. `none` has no feature equivalent: it's the escape hatch
    // for hosts that already export the symbols or dlopen them at run time.
    if let Ok(value) = env::var("GODOT_LINK") {
        return match value.to_ascii_lowercase().as_str() {
            "static" => LinkMode::Static,
            "dynamic" | "dylib" | "shared" => LinkMode::Dynamic,
            "none" | "off" => LinkMode::None,
            other => panic!("GODOT_LINK must be static, dynamic or none (got {other:?})"),
        };
    }

    // Static is the unconditional default; the feature only opts out of it.
    if env::var_os("CARGO_FEATURE_DYNAMIC_LINK").is_some() {
        LinkMode::Dynamic
    } else {
        LinkMode::Static
    }
}

/// Godot is C++ and drags in the platform runtime; a shared library carries
/// these itself, a static archive does not.
fn link_static_dependencies() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    match target_os.as_str() {
        "linux" | "freebsd" | "netbsd" | "openbsd" => {
            println!("cargo:rustc-link-lib=dylib=stdc++");
            println!("cargo:rustc-link-lib=dylib=m");
            println!("cargo:rustc-link-lib=dylib=dl");
            println!("cargo:rustc-link-lib=dylib=pthread");
            // Godot's linuxbsd build defaults to use_sowrap=yes, which dlopens
            // X11/Wayland/ALSA/PulseAudio/udev/dbus. Built with use_sowrap=no,
            // add them through GODOT_EXTRA_LIBS.
        }
        "macos" | "ios" => {
            println!("cargo:rustc-link-lib=dylib=c++");
            for framework in [
                "AppKit",
                "AudioUnit",
                "AVFoundation",
                "CoreAudio",
                "CoreGraphics",
                "CoreHaptics",
                "CoreMedia",
                "CoreMIDI",
                "CoreVideo",
                "Foundation",
                "GameController",
                "IOKit",
                "Metal",
                "QuartzCore",
                "Security",
            ] {
                println!("cargo:rustc-link-lib=framework={framework}");
            }
        }
        "windows" => {
            if target_env == "gnu" {
                println!("cargo:rustc-link-lib=dylib=stdc++");
            }
            for lib in [
                "advapi32", "bcrypt", "comdlg32", "dbghelp", "dinput8", "dwmapi", "gdi32", "imm32",
                "iphlpapi", "ole32", "oleaut32", "setupapi", "shell32", "shlwapi", "user32",
                "uuid", "version", "winmm", "ws2_32",
            ] {
                println!("cargo:rustc-link-lib=dylib={lib}");
            }
        }
        other => {
            println!(
                "cargo:warning=no default system libraries known for target_os={other}; \
                      use GODOT_EXTRA_LIBS if the link fails"
            );
        }
    }
}

/// How well a library's name fits this build. Ordered worst to best.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Match {
    /// Some other naming scheme, e.g. a renamed library; might still work.
    Unknown,
    /// Right platform and arch, other target (say, an editor build).
    OtherTarget,
    /// Right platform, arch and target.
    Exact,
}

/// Godot's artifacts carry the full build config in the name, e.g.
/// `libgodot.linuxbsd.template_debug.x86_64.a` (with `.dev`, `.double` and
/// friends possibly mixed in), so probe the directory and rank what's there.
/// Ties go to the newest file, which after a scons run is the one it wrote.
fn find_lib(dir: &Path, mode: LinkMode, target: &str) -> Option<(Match, String)> {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let windows = target_os == "windows";
    let platform = godot_platform();
    let arch = godot_arch();

    let ext = match (mode, target_os.as_str(), windows) {
        (LinkMode::Static, _, true) => "lib",
        (LinkMode::Static, _, false) => "a",
        (LinkMode::Dynamic, _, true) => "lib", // MSVC import library next to the .dll
        (LinkMode::Dynamic, "macos", _) | (LinkMode::Dynamic, "ios", _) => "dylib",
        (LinkMode::Dynamic, _, _) => "so",
        (LinkMode::None, ..) => return None,
    };

    let mut found: Vec<(Match, SystemTime, String)> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let stem = name.strip_suffix(&format!(".{ext}"))?;
            // `-l foo` resolves libfoo.a / foo.lib, so hand back the bare stem.
            let stem = stem.strip_prefix("lib").unwrap_or(stem);
            if !stem.starts_with("godot") {
                return None;
            }
            // On Windows both the static lib and the DLL's import lib end in
            // .lib; only the latter is named *.dll.lib.
            if windows && stem.ends_with(".dll") != (mode == LinkMode::Dynamic) {
                return None;
            }

            let parts: Vec<&str> = stem.split('.').collect();
            let rank = match parts.as_slice() {
                ["godot", p, t, rest @ ..] if Some(*p) == platform && arch.is_some_and(|a| rest.contains(&a)) => {
                    if *t == target { Match::Exact } else { Match::OtherTarget }
                }
                _ => Match::Unknown,
            };
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((rank, modified, stem.to_string()))
        })
        .collect();

    found.sort();
    let (rank, _, name) = found.pop()?;
    let rivals: Vec<&str> = found.iter().filter(|(r, ..)| *r == rank).map(|(.., n)| n.as_str()).collect();
    if !rivals.is_empty() {
        println!(
            "cargo:warning=several Godot libraries fit in {}; using the newest, {name} (also: {rivals:?}). \
             Set GODOT_LIB_NAME to choose.",
            dir.display()
        );
    }
    Some((rank, name))
}
