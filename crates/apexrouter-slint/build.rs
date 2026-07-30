//! OWNER: unit S0 (crates/apexrouter-slint/build.rs). One line, by design.

fn main() {
    slint_build::compile("src/ui/appwindow.slint").expect("compile src/ui/appwindow.slint");
}
