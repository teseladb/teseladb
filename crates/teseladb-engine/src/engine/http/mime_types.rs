//! File extension to MIME type mapping for the file handlers.
//!
//! [`mime_types::extension_to_type`] looks the extension up in a small static table and
//! falls back to `text/plain`; the lookup does not allocate. The `httpd` module keeps the
//! same function under its older path.

/// The extension to MIME type table and its lookup.
#[allow(clippy::module_inception)]
pub mod mime_types {
    struct Mapping {
        extension: &'static str,
        mime_type: &'static str,
    }

    static MAPPINGS: [Mapping; 14] = [
        Mapping { extension: "json", mime_type: "application/json" },
        Mapping { extension: "xml", mime_type: "application/xml" },
        Mapping { extension: "gif", mime_type: "image/gif" },
        Mapping { extension: "htm", mime_type: "text/html" },
        Mapping { extension: "css", mime_type: "text/css" },
        Mapping { extension: "js", mime_type: "text/javascript" },
        Mapping { extension: "html", mime_type: "text/html" },
        Mapping { extension: "jpg", mime_type: "image/jpeg" },
        Mapping { extension: "svg", mime_type: "image/svg+xml" },
        Mapping { extension: "png", mime_type: "image/png" },
        Mapping { extension: "txt", mime_type: "text/plain" },
        Mapping { extension: "ico", mime_type: "image/x-icon" },
        Mapping { extension: "bin", mime_type: "application/octet-stream" },
        Mapping {
            extension: "proto",
            mime_type: "application/vnd.google.protobuf; proto=io.prometheus.client.MetricFamily; encoding=delimited",
        },
    ];

    /// Returns the MIME type of the file extension `extension`, or `text/plain` for an
    /// unknown extension.
    pub fn extension_to_type(extension: &str) -> &'static str {
        for m in MAPPINGS.iter() {
            if extension == m.extension {
                return m.mime_type;
            }
        }
        "text/plain"
    }
}
/// The lookup under its older `httpd::mime_types` path.
pub mod httpd {
    /// Forwarders to [`super::mime_types`].
    pub mod mime_types {
        /// Convert a file extension into a MIME type.
        #[deprecated(note = "Use http::mime_types::extension_to_type instead")]
        #[inline]
        pub fn extension_to_type(extension: &str) -> &'static str {
            super::super::mime_types::extension_to_type(extension)
        }
    }
}
