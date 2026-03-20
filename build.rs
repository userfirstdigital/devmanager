fn main() {
    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("packaging/icons/devmanager.ico");
        res.compile().expect("failed to compile windows resources");
    }
}
