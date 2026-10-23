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

//! Forces an actual link against the engine.
//!
//! A library crate compiles to an rlib, so nothing is linked while building it
//! — the linker only runs for a binary. Run this to verify the configuration:
//!
//!     cargo run --example link_check                          # static
//!     cargo run --example link_check --features dynamic-link   # dynamic

fn main() {
    println!("linkage: {}", libgodot::LINKAGE);

    // Referencing the symbols is what drags the engine in at link time.
    let create = libgodot::libgodot_create_godot_instance as *const ();
    let destroy = libgodot::libgodot_destroy_godot_instance as *const ();
    println!("libgodot_create_godot_instance:  {create:p}");
    println!("libgodot_destroy_godot_instance: {destroy:p}");
}
