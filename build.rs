fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Course-snatching");
        res.set("FileDescription", "Course-snatching");
        res.set("ProductVersion", env!("CARGO_PKG_VERSION"));
        res.compile().expect("failed to compile Windows resources");
    }
}
