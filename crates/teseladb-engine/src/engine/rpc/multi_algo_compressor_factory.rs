//! Negotiation of the RPC compression algorithm among several.
//!
//! [`MultiAlgoCompressorFactory`] advertises the names of the algorithms of its factories,
//! comma-separated in order of preference, and on connect picks the first algorithm of the
//! client's list that the other side also supports, returning that factory's compressor.

use crate::engine::core::sstring::Sstring;
use crate::engine::rpc::rpc_types::{Compressor, CompressorFactory, SendEmptyFrame};

// This is meta compressor factory. It gets an array of regular factories that
// support one compression algorithm each and negotiates common compression algorithm
// that is supported both by a client and a server. The order of algorithm preferences
// is the order they appear in clien's list
/// Negotiates the first algorithm of a list that both sides support.
pub struct MultiAlgoCompressorFactory {
    _factories: Vec<&'static dyn CompressorFactory>,
    _features: Sstring,
}

impl MultiAlgoCompressorFactory {
    /// A factory over `factories`, in order of preference.
    pub fn new(factories: Vec<&'static dyn CompressorFactory>) -> Self {
        let mut features = Sstring::new();
        for (i, f) in factories.iter().enumerate() {
            if i != 0 {
                features.append_bytes(b",");
            }
            features.append_bytes(f.supported().as_bytes());
        }
        MultiAlgoCompressorFactory { _factories: factories, _features: features }
    }

    /// A factory over one factory.
    pub fn from_one(factory: &'static dyn CompressorFactory) -> Self {
        Self::new(vec![factory])
    }

    fn negotiate_names(
        &self,
        feature: &Sstring,
        is_server: bool,
        send_empty_frame: &dyn Fn() -> SendEmptyFrame,
    ) -> Option<Box<dyn Compressor>> {
        let names = feature.as_bytes().split(|&c| c == b',');
        if is_server {
            for n in names {
                for f in &self._factories {
                    if let Some(c) =
                        f.negotiate_with(Sstring::from_bytes(n), is_server, send_empty_frame())
                    {
                        return Some(c);
                    }
                }
            }
        } else {
            for f in &self._factories {
                for n in feature.as_bytes().split(|&c| c == b',') {
                    if let Some(c) =
                        f.negotiate_with(Sstring::from_bytes(n), is_server, send_empty_frame())
                    {
                        return Some(c);
                    }
                }
            }
        }
        None
    }
}

impl CompressorFactory for MultiAlgoCompressorFactory {
    // return feature string that will be sent as part of protocol negotiation
    fn supported(&self) -> &Sstring {
        &self._features
    }

    // negotiate compress algorithm
    fn negotiate(&self, feature: Sstring, is_server: bool) -> Option<Box<dyn Compressor>> {
        // No empty-frame sender was given: one that fails with "call to an empty function" stands in,
        // so a compressor that tries to send an empty frame gets an error.
        self.negotiate_names(&feature, is_server, &|| -> SendEmptyFrame {
            Box::new(|| {
                crate::engine::core::future::make_exception_future(
                    crate::engine::core::future::ExceptionPtr::new(
                        crate::engine::core::future::RuntimeError(
                            "call to an empty function".to_string(),
                        ),
                    ),
                )
            })
        })
    }

    fn negotiate_with(
        &self,
        feature: Sstring,
        is_server: bool,
        send_empty_frame: SendEmptyFrame,
    ) -> Option<Box<dyn Compressor>> {
        // Each candidate gets its own sender; all of them share the one given here.
        let shared = std::rc::Rc::new(send_empty_frame);
        self.negotiate_names(&feature, is_server, &|| -> SendEmptyFrame {
            let s = shared.clone();
            Box::new(move || s())
        })
    }
}
