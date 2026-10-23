# libgodot

Embed the Godot engine in a Rust program through
[libgodot](https://github.com/godotengine/godot/blob/master/core/extension/libgodot.h)
(Godot 4.6 and newer).

```rust
fn main() -> Result<(), libgodot::Error> {
    let mut godot = libgodot::GodotInstance::create(["--headless", "--path", "my_project"])?;
    godot.run() // start, then run frames until the project quits
}
```

`GodotInstance` also has `start`, `iteration` (one frame, for driving the
loop yourself), `pause`/`resume` and `focus_in`/`focus_out`. Godot can only be
created once per process, and it has to stay on the thread that created it.
To register your own classes, pass your GDExtension entry point to
`GodotInstance::create_with_init`. The raw bindings are in `libgodot::sys`.

Try it with the bundled example:

```sh
cargo run --example run_project -- --headless --path path/to/project
```

## Platform support

Only tested on Linux, including a full engine build with `build-godot` and
static and dynamic linking. Windows and macOS should work in theory: the build
knows their system libraries, scons platforms and archivers (`lib.exe` and
`libtool` for merging the static archives), but none of it has been run there
yet. Reports and fixes are welcome.

## Where the engine comes from

The build needs the Godot source (for the headers) and a compiled Godot
library (to link). It looks for the source in this order:

1. `GODOT_SOURCE_PATH`, if set.
2. `./godot` next to `Cargo.toml` (a checkout or a symlink to one).
3. A shallow clone, cached under `target/godot-src/<tag>/` (or
   `GODOT_CLONE_DIR`). Each version is cloned once; delete its directory to
   refresh `latest`.

Pick the cloned version with one feature, or with `GODOT_VERSION`, which
overrides the features and also takes any tag or branch, e.g. `4.6.2-stable`:

| Feature        | `GODOT_VERSION` | Clones                          |
| -------------- | --------------- | ------------------------------- |
| `godot-stable` | `stable`        | newest stable release (default) |
| `godot-latest` | `latest`        | `master`                        |
| `godot-4-6`    | `4.6`           | newest `4.6.x-stable`           |
| `godot-4-7`    | `4.7`           | newest `4.7.x-stable`           |

Offline (`CARGO_NET_OFFLINE=true`), the build uses a cached clone that matches,
and otherwise falls back to the checked-in `src/bindings.rs`, as it also does
on docs.rs.

## Building the engine

Enable `build-godot` and the build runs scons in the source tree when
`bin/` has no library for the current build:

```sh
cargo build --features build-godot
SCONSFLAGS="use_lto=yes -j8" cargo build --release --features build-godot
```

It passes `platform` and `arch` for the Cargo target, `library_type` for the
link mode, and `target=template_debug` (or `template_release` for
`--release`). Anything set in `SCONSFLAGS` wins over these. The first build
takes a long time; run `cargo build -vv` to watch it. You need scons and Python
(`SCONS` and `PYTHON` choose the executables).

Without the feature, build it yourself, e.g.
`scons platform=linuxbsd target=template_debug library_type=static_library`.

## Linking

Linking is static by default; the `dynamic-link` feature links the shared
library instead, which then has to be found at run time (`LD_LIBRARY_PATH`,
rpath, ...). A static build of Godot is spread over dozens of archives under
`bin/obj/`; they are merged into a single archive in the build directory so
any binary can link them.

The library is picked from `bin/` by its name: the one built for this
platform, architecture and target first, else another target for the same
platform and architecture (with a warning), so an editor build still works
when it is all you have.

## Environment variables

| Variable                | Effect                                                                                  |
| ----------------------- | --------------------------------------------------------------------------------------- |
| `GODOT_SOURCE_PATH`     | Godot source tree to use.                                                               |
| `GODOT_VERSION`         | Version to clone; overrides the `godot-*` features.                                     |
| `GODOT_CLONE_DIR`       | Where clones are kept (default `target/godot-src`).                                     |
| `GODOT_LINK`            | `static`, `dynamic` or `none` (no link directives); overrides `dynamic-link`.           |
| `GODOT_TARGET`          | `editor`, `template_debug` or `template_release` to link and build.                     |
| `GODOT_LIB_DIR`         | Directory holding a prebuilt library, instead of the source tree's `bin/`.              |
| `GODOT_LIB_NAME`        | Exact library name to link (without `lib` prefix or extension).                         |
| `GODOT_WHOLE_ARCHIVE`   | `1` to link the static library with `+whole-archive`.                                   |
| `GODOT_EXTRA_LIBS`      | Extra libraries, comma-separated, e.g. `dylib=X11,static=embree3`.                      |
| `GODOT_RPATH`           | `1` to add the library directory to the rpath (this crate's examples and tests only).   |
| `GODOT_UPDATE_BINDINGS` | `1` to copy freshly generated bindings over the checked-in `src/bindings.rs`.           |
| `SCONSFLAGS`            | Extra scons options for `build-godot`; scons reads it itself.                           |
| `SCONS`, `PYTHON`, `AR` | Executables to use for scons, Python and archive merging.                               |

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

Godot itself is under the MIT license; see its
[COPYRIGHT.txt](https://github.com/godotengine/godot/blob/master/COPYRIGHT.txt)
for the engine and its third-party components.


## llm notice
This project was developed with help my a LLM
