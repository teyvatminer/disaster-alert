use html_minifier::HTMLMinifier;
use std::{env, fs, io, path::PathBuf};

const INSTANCE_NOTICE_MARKER: &[u8] = b"__DISASTER_ALERT_INSTANCE_NOTICE__";

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=web/index.html");

    let source = fs::read("web/index.html")?;
    if source
        .windows(INSTANCE_NOTICE_MARKER.len())
        .filter(|window| *window == INSTANCE_NOTICE_MARKER)
        .count()
        != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "web/index.html must contain exactly one instance notice marker",
        ));
    }
    let mut minifier = HTMLMinifier::new();
    minifier.digest(source).map_err(io::Error::other)?;

    let output = PathBuf::from(
        env::var_os("OUT_DIR")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Cargo did not set OUT_DIR"))?,
    )
    .join("index.min.html");
    fs::write(output, minifier.get_html())
}
