// CPython 3.11 `mimetypes` default tables + `guess_type`.
//
// python `web/app.py:135` types the `/<id>/file/<path>` proxy with
// `mimetypes.guess_type(path)` and falls back to `application/octet-stream`,
// so the proxy's content-type IS this table. Swift used a hand-written
// nine-case switch, which typed `.pdf`, `.csv`, `.wasm`, `.ico`, `.py` and
// every other extension wrong.
//
// Generated from `Lib/mimetypes.py` (`_suffix_map_default`,
// `_encodings_map_default`, `_types_map_default`); the same tables the rust
// port carries in `mimetypes_table.rs`.

import Foundation

enum Mimetypes {
    /// `_suffix_map_default` — a compound suffix rewritten before the lookup.
    static let suffixMap: [String: String] = [
        ".svgz": ".svg.gz",
        ".tgz": ".tar.gz",
        ".taz": ".tar.gz",
        ".tz": ".tar.gz",
        ".tbz2": ".tar.bz2",
        ".txz": ".tar.xz",
    ]

    /// `_encodings_map_default` — a suffix that names an ENCODING, stripped
    /// before the type lookup (`.tar.gz` types as `application/x-tar`).
    static let encodingsMap: Set<String> = [".gz", ".Z", ".bz2", ".xz", ".br"]

    /// `_types_map_default` — extension → media type, lowercase keys.
    static let typesMap: [String: String] = [
        ".js": "application/javascript",
        ".mjs": "application/javascript",
        ".json": "application/json",
        ".webmanifest": "application/manifest+json",
        ".doc": "application/msword",
        ".dot": "application/msword",
        ".wiz": "application/msword",
        ".nq": "application/n-quads",
        ".nt": "application/n-triples",
        ".bin": "application/octet-stream",
        ".a": "application/octet-stream",
        ".dll": "application/octet-stream",
        ".exe": "application/octet-stream",
        ".o": "application/octet-stream",
        ".obj": "application/octet-stream",
        ".so": "application/octet-stream",
        ".oda": "application/oda",
        ".pdf": "application/pdf",
        ".p7c": "application/pkcs7-mime",
        ".ps": "application/postscript",
        ".ai": "application/postscript",
        ".eps": "application/postscript",
        ".trig": "application/trig",
        ".m3u": "application/vnd.apple.mpegurl",
        ".m3u8": "application/vnd.apple.mpegurl",
        ".xls": "application/vnd.ms-excel",
        ".xlb": "application/vnd.ms-excel",
        ".ppt": "application/vnd.ms-powerpoint",
        ".pot": "application/vnd.ms-powerpoint",
        ".ppa": "application/vnd.ms-powerpoint",
        ".pps": "application/vnd.ms-powerpoint",
        ".pwz": "application/vnd.ms-powerpoint",
        ".wasm": "application/wasm",
        ".bcpio": "application/x-bcpio",
        ".cpio": "application/x-cpio",
        ".csh": "application/x-csh",
        ".dvi": "application/x-dvi",
        ".gtar": "application/x-gtar",
        ".hdf": "application/x-hdf",
        ".h5": "application/x-hdf5",
        ".latex": "application/x-latex",
        ".mif": "application/x-mif",
        ".cdf": "application/x-netcdf",
        ".nc": "application/x-netcdf",
        ".p12": "application/x-pkcs12",
        ".pfx": "application/x-pkcs12",
        ".ram": "application/x-pn-realaudio",
        ".pyc": "application/x-python-code",
        ".pyo": "application/x-python-code",
        ".sh": "application/x-sh",
        ".shar": "application/x-shar",
        ".swf": "application/x-shockwave-flash",
        ".sv4cpio": "application/x-sv4cpio",
        ".sv4crc": "application/x-sv4crc",
        ".tar": "application/x-tar",
        ".tcl": "application/x-tcl",
        ".tex": "application/x-tex",
        ".texi": "application/x-texinfo",
        ".texinfo": "application/x-texinfo",
        ".roff": "application/x-troff",
        ".t": "application/x-troff",
        ".tr": "application/x-troff",
        ".man": "application/x-troff-man",
        ".me": "application/x-troff-me",
        ".ms": "application/x-troff-ms",
        ".ustar": "application/x-ustar",
        ".src": "application/x-wais-source",
        ".xsl": "application/xml",
        ".rdf": "application/xml",
        ".wsdl": "application/xml",
        ".xpdl": "application/xml",
        ".zip": "application/zip",
        ".3gp": "audio/3gpp",
        ".3gpp": "audio/3gpp",
        ".3g2": "audio/3gpp2",
        ".3gpp2": "audio/3gpp2",
        ".aac": "audio/aac",
        ".adts": "audio/aac",
        ".loas": "audio/aac",
        ".ass": "audio/aac",
        ".au": "audio/basic",
        ".snd": "audio/basic",
        ".mp3": "audio/mpeg",
        ".mp2": "audio/mpeg",
        ".opus": "audio/opus",
        ".aif": "audio/x-aiff",
        ".aifc": "audio/x-aiff",
        ".aiff": "audio/x-aiff",
        ".ra": "audio/x-pn-realaudio",
        ".wav": "audio/x-wav",
        ".avif": "image/avif",
        ".bmp": "image/bmp",
        ".gif": "image/gif",
        ".ief": "image/ief",
        ".jpg": "image/jpeg",
        ".jpe": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".heic": "image/heic",
        ".heif": "image/heif",
        ".png": "image/png",
        ".svg": "image/svg+xml",
        ".tiff": "image/tiff",
        ".tif": "image/tiff",
        ".ico": "image/vnd.microsoft.icon",
        ".ras": "image/x-cmu-raster",
        ".pnm": "image/x-portable-anymap",
        ".pbm": "image/x-portable-bitmap",
        ".pgm": "image/x-portable-graymap",
        ".ppm": "image/x-portable-pixmap",
        ".rgb": "image/x-rgb",
        ".xbm": "image/x-xbitmap",
        ".xpm": "image/x-xpixmap",
        ".xwd": "image/x-xwindowdump",
        ".eml": "message/rfc822",
        ".mht": "message/rfc822",
        ".mhtml": "message/rfc822",
        ".nws": "message/rfc822",
        ".css": "text/css",
        ".csv": "text/csv",
        ".html": "text/html",
        ".htm": "text/html",
        ".n3": "text/n3",
        ".txt": "text/plain",
        ".bat": "text/plain",
        ".c": "text/plain",
        ".h": "text/plain",
        ".ksh": "text/plain",
        ".pl": "text/plain",
        ".srt": "text/plain",
        ".rtx": "text/richtext",
        ".tsv": "text/tab-separated-values",
        ".vtt": "text/vtt",
        ".py": "text/x-python",
        ".etx": "text/x-setext",
        ".sgm": "text/x-sgml",
        ".sgml": "text/x-sgml",
        ".vcf": "text/x-vcard",
        ".xml": "text/xml",
        ".mp4": "video/mp4",
        ".mpeg": "video/mpeg",
        ".m1v": "video/mpeg",
        ".mpa": "video/mpeg",
        ".mpe": "video/mpeg",
        ".mpg": "video/mpeg",
        ".mov": "video/quicktime",
        ".qt": "video/quicktime",
        ".webm": "video/webm",
        ".avi": "video/x-msvideo",
        ".movie": "video/x-sgi-movie",
    ]

    /// `posixpath.splitext` — split at the last dot that follows the last `/`,
    /// skipping a run of leading dots in the basename (so `.bashrc` has no
    /// extension and `..a` splits at the final dot).
    static func splitExt(_ path: String) -> (base: String, ext: String) {
        let c = Array(path)
        let sepIndex = c.lastIndex(of: "/") ?? -1
        guard let dotIndex = c.lastIndex(of: "."), dotIndex > sepIndex else { return (path, "") }
        var i = sepIndex + 1
        while i < dotIndex {
            if c[i] != "." {
                return (String(c[..<dotIndex]), String(c[dotIndex...]))
            }
            i += 1
        }
        return (path, "")
    }

    /// `urllib.parse._splittype` — `<scheme>:<rest>` where the scheme has no
    /// `/` or `:`. Returns `(nil, url)` when there is no scheme.
    static func splitType(_ url: String) -> (scheme: String?, rest: String) {
        guard let colon = url.firstIndex(of: ":") else { return (nil, url) }
        let scheme = String(url[..<colon])
        if scheme.isEmpty || scheme.contains("/") { return (nil, url) }
        return (scheme.lowercased(), String(url[url.index(after: colon)...]))
    }

    /// CPython `mimetypes.guess_type(url)[0]` (strict). `nil` for an extension
    /// the default table does not name.
    static func guessType(_ path: String) -> String? {
        let (scheme, url) = splitType(path)
        if scheme == "data" {
            // dataurl := "data:" [ mediatype ] [ ";base64" ] "," data
            guard let comma = url.firstIndex(of: ",") else { return nil }
            let head = url[..<comma]
            let type = head.firstIndex(of: ";").map { String(head[..<$0]) } ?? String(head)
            if type.contains("=") || !type.contains("/") { return "text/plain" }
            return type
        }
        var (base, ext) = splitExt(url)
        while let mapped = suffixMap[ext.lowercased()] {
            (base, ext) = splitExt(base + mapped)
        }
        // `encodings_map` is case SENSITIVE (CPython says so in a comment):
        // `.Z` strips, `.z` does not.
        if encodingsMap.contains(ext) {
            (base, ext) = splitExt(base)
        }
        _ = base
        return typesMap[ext.lowercased()]
    }
}
