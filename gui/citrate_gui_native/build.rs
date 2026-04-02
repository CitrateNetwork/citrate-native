fn main() {
    if let Err(err) = slint_build::compile("ui/app.slint") {
        panic!("Slint UI compilation failed: {err}");
    }
}
