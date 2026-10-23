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

//! Runs a Godot project inside this process until it quits.
//!
//!     cargo run --example run_project -- --headless --path path/to/project
//!
//! Arguments are passed to Godot as if given on its command line.

fn main() -> Result<(), libgodot::Error> {
    let mut godot = libgodot::GodotInstance::create(std::env::args().skip(1))?;
    godot.run()
}
