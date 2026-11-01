//! The stemmers of the `snowballStemming` token filter, written from the algorithm descriptions
//! of snowballstem.org (research R7) and checked word by word against the stems `refsearch` gives
//! for the Snowball vocabularies (`tests/fixtures/refsearch/stem/`).

mod arabic;
mod armenian;
mod basque;
mod catalan;
mod dutch;
mod english;
mod estonian;
mod finnish;
mod french;
mod german;
mod hungarian;
mod irish;
mod italian;
mod lithuanian;
mod nordic;
mod porter;
mod portuguese;
mod romanian;
mod russian;
mod spanish;
mod table;
mod turkish;
mod word;

pub use word::Word;

/// Declares the stemmers: variant, `stemmerName` and stemming function.
macro_rules! stemmers {
    ($($variant:ident => $name:literal, $stem:expr;)*) => {
        /// A `stemmerName` of `snowballStemming`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Stemmer {
            $(
                #[doc = concat!("`", $name, "`.")]
                $variant,
            )*
        }

        /// Every stemmer with its `stemmerName`.
        pub const STEMMERS: &[(Stemmer, &str)] = &[$((Stemmer::$variant, $name)),*];

        impl Stemmer {
            /// The stem of `word`.
            pub fn stem(self, word: &str) -> String {
                match self {
                    $(Stemmer::$variant => $stem(word),)*
                }
            }
        }
    };
}

stemmers! {
    Arabic => "arabic", arabic::stem;
    Armenian => "armenian", armenian::stem;
    Basque => "basque", basque::stem;
    Catalan => "catalan", catalan::stem;
    Danish => "danish", nordic::danish;
    Dutch => "dutch", dutch::stem;
    English => "english", english::stem;
    Estonian => "estonian", estonian::stem;
    Finnish => "finnish", finnish::stem;
    French => "french", french::stem;
    German => "german", |w| german::stem(w, true);
    German2 => "german2", |w| german::stem(w, true);
    Hungarian => "hungarian", hungarian::stem;
    Irish => "irish", irish::stem;
    Italian => "italian", italian::stem;
    Lithuanian => "lithuanian", lithuanian::stem;
    Norwegian => "norwegian", nordic::norwegian;
    Porter => "porter", |w| porter::stem(w, porter::Variant::Snowball);
    Portuguese => "portuguese", portuguese::stem;
    Romanian => "romanian", romanian::stem;
    Russian => "russian", russian::stem;
    Spanish => "spanish", spanish::stem;
    Swedish => "swedish", nordic::swedish;
    Turkish => "turkish", turkish::stem;
}

impl Stemmer {
    /// The stemmer called `name`.
    pub fn by_name(name: &str) -> Option<Stemmer> {
        STEMMERS.iter().find(|(_, n)| *n == name).map(|(s, _)| *s)
    }

    /// The `stemmerName`.
    pub fn name(self) -> &'static str {
        STEMMERS.iter().find(|(s, _)| *s == self).map_or("", |(_, n)| n)
    }
}

/// The stem the `porterStemming` token filter gives `word`.
pub fn porter_stemming(word: &str) -> String {
    porter::stem(word, porter::Variant::Filter)
}
