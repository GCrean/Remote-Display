// Embed the app icon in the Windows executable (taskbar, title bar, Explorer). GPUI loads icon
// resource id 1 for its windows, which is what `set_icon` writes.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/icon.ico");
        res.set("ProductName", "Remote Display");
        res.set("FileDescription", "Remote Display");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
