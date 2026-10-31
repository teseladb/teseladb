//! Values written the way the server writes them inside error messages: the printer lives in
//! `teseladb-bson` ([`teseladb_bson::shell`]), shared with the collations' messages.

pub use teseladb_bson::shell::{double, general, shell_doc, shell_value};
