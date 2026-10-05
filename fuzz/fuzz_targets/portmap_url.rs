//! The URLs a gateway hands out, and the references its description resolves
//! against them: a base, a line feed, then a reference. Every URL taken is a
//! path from the root with no dot segment and nothing that could end a
//! request line, and reads back as itself.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::url::Url;

fn check(url: &Url) {
    assert!(url.path.starts_with('/'), "a path not from the root");
    assert!(
        url.path.bytes().all(|b| b.is_ascii_graphic()),
        "a byte that could end a request line"
    );
    let path = url
        .path
        .split_once('?')
        .map_or(url.path.as_str(), |(path, _)| path);
    assert!(
        !path
            .split('/')
            .any(|segment| segment == "." || segment == ".."),
        "a dot segment left in {path:?}"
    );
    let again = Url::parse(&format!("http://{}{}", url.addr, url.path));
    assert_eq!(
        again.as_ref(),
        Ok(url),
        "a URL that does not read back as itself"
    );
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (base, reference) = text.split_once('\n').unwrap_or((text, ""));
    let Ok(base) = Url::parse(base) else {
        return;
    };
    check(&base);
    if let Ok(joined) = base.join(reference) {
        check(&joined);
    }
});
