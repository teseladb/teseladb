//! Handlers that serve files from disk.
//!
//! [`FileHandler`] serves one fixed file and [`DirectoryHandler`] serves the files under a
//! document root, using the request path. Both embed a [`FileInteractionHandler`], which
//! holds the optional [`FileTransformer`] applied to the body, adds the trailing-slash
//! redirect and streams the file with DMA reads into the reply's output stream. The MIME
//! type comes from the file extension.

use std::cell::RefCell;
use std::rc::Rc;

use crate::engine::core::do_with::do_with;
use crate::engine::core::file::DirectoryEntryType;
use crate::engine::core::file::{File, OpenFlags, open_file_dma};
use crate::engine::core::fstream::{FileInputStreamOptions, make_file_input_stream};
use crate::engine::core::future::{Future, make_ready_future};
use crate::engine::core::iostream::{InputStream, OutputStream, copy};
use crate::engine::core::reactor::engine;
use crate::engine::core::sstring::Sstring;
use crate::engine::http::handlers::{HandlerBase, HandlerBaseData};
use crate::engine::http::reply::{BodyWriter, Reply, StatusType};
use crate::engine::http::request::Request;
use crate::engine::util::string_utils::internal::CaseInsensitiveKey;

/// A transformer of file contents before they are returned in a reply.
///
/// It acts as a factory of output streams: from the file extension it decides whether a
/// transformation is needed, and if so wraps the reply's stream in a new one that
/// transforms what is written through it.
pub trait FileTransformer {
    /// Returns the output stream to write the file to the reply through: `s` itself, or a
    /// new stream that transforms the content before writing it to `s`.
    ///
    /// `req` is the request and `extension` the extension of the file the content comes
    /// from.
    fn transform(
        &mut self,
        req: Box<Request>,
        extension: &Sstring,
        s: OutputStream<u8>,
    ) -> OutputStream<u8>;
}

/// The file logic that [`DirectoryHandler`] and [`FileHandler`] share.
///
/// Both read a file from disk, optionally transform it, and return the result, or a
/// not-found reply on error.
pub struct FileInteractionHandler {
    base: HandlerBaseData,
    /// The transformer of the files returned. The handler shares it with the body writers
    /// of its replies, which run after the handler returns and may outlive it.
    transformer: Option<SharedTransformer>,
}

/// A transformer shared by a handler and the body writers of its replies.
type SharedTransformer = Rc<RefCell<Box<dyn FileTransformer>>>;

impl FileInteractionHandler {
    /// A handler with an optional transformer.
    pub fn new(p: Option<Box<dyn FileTransformer>>) -> Self {
        FileInteractionHandler {
            base: HandlerBaseData::default(),
            transformer: p.map(|t| Rc::new(RefCell::new(t))),
        }
    }

    /// Sets the transformer applied to the files returned (`None` for none), and returns
    /// `self`. Replies already being written keep the transformer they started with.
    pub fn set_transformer(&mut self, t: Option<Box<dyn FileTransformer>>) -> &mut Self {
        self.transformer = t.map(|t| Rc::new(RefCell::new(t)));
        self
    }

    /// Redirects to the same URL with a trailing slash when the request URL does not end
    /// with one.
    ///
    /// Sets `rep` to a `301 Moved Permanently` with the new `Location` and returns
    /// `true` when it redirects; returns `false` otherwise.
    pub fn redirect_if_needed(&self, req: &Request, rep: &mut Reply) -> bool {
        redirect_if_needed(req, rep)
    }

    /// Returns the extension of the file path `file`, in lowercase: what follows the last
    /// dot when that dot is in the last path component (after the last `/`, or anywhere in a
    /// path without one). Returns an empty string when there is no such dot.
    pub fn get_extension(file: &Sstring) -> Sstring {
        get_extension(file)
    }

    /// Returns the file at the full path `file_name` in the reply `rep`.
    ///
    /// The reply's content type comes from the file extension, and its body writer opens
    /// the file and copies it, through the transformer if one is set, when the reply is
    /// sent. The body writer holds the transformer itself, so the handler may go first.
    pub fn read(
        &mut self,
        file_name: Sstring,
        req: Box<Request>,
        rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        read_file(self.transformer.clone(), file_name, req, rep)
    }

    /// The stream to write the file into: `s`, or its transformation.
    pub fn get_stream(
        &mut self,
        req: Box<Request>,
        extension: &Sstring,
        s: OutputStream<u8>,
    ) -> OutputStream<u8> {
        transformed_stream(self.transformer.as_ref(), req, extension, s)
    }
}

/// See [`FileInteractionHandler::redirect_if_needed`].
fn redirect_if_needed(req: &Request, rep: &mut Reply) -> bool {
    // The slash goes at the end of the path, before the query if there is one: a slash in the
    // query neither counts nor is the place for it.
    let location = req.get_url();
    let b = location.as_bytes();
    let insert_pos = b.iter().position(|&c| c == b'?').unwrap_or(b.len());
    if insert_pos == 0 || b[insert_pos - 1] != b'/' {
        let mut redirected = Vec::with_capacity(b.len() + 1);
        redirected.extend_from_slice(&b[..insert_pos]);
        redirected.push(b'/');
        redirected.extend_from_slice(&b[insert_pos..]);
        rep.set_status(StatusType::MOVED_PERMANENTLY, Sstring::new());
        rep._headers.insert(
            CaseInsensitiveKey(Sstring::from("Location")),
            Sstring::from_bytes(&redirected),
        );
        rep.done();
        return true;
    }
    false
}

/// See [`FileInteractionHandler::get_extension`].
fn get_extension(file: &Sstring) -> Sstring {
    let b = file.as_bytes();
    let last_slash_pos = b.iter().rposition(|&c| c == b'/');
    let last_dot_pos = b.iter().rposition(|&c| c == b'.');
    let mut extension = Sstring::new();
    // The dot must be in the last path component: after the last slash, or anywhere in a bare
    // file name (a relative path without a slash).
    if let Some(dot) = last_dot_pos
        && last_slash_pos.is_none_or(|slash| dot > slash)
    {
        extension = Sstring::from_bytes(&b[dot + 1..]);
    }
    // normalize file extension for mime type
    extension.as_bytes_mut().make_ascii_lowercase();
    extension
}

/// See [`FileInteractionHandler::get_stream`].
fn transformed_stream(
    transformer: Option<&SharedTransformer>,
    req: Box<Request>,
    extension: &Sstring,
    s: OutputStream<u8>,
) -> OutputStream<u8> {
    match transformer {
        Some(t) => t.borrow_mut().transform(req, extension, s),
        None => s,
    }
}

/// See [`FileInteractionHandler::read`]: the body writer owns what it needs, the
/// transformer included.
fn read_file(
    transformer: Option<SharedTransformer>,
    file_name: Sstring,
    req: Box<Request>,
    mut rep: Box<Reply>,
) -> Future<Box<Reply>> {
    let extension = get_extension(&file_name);
    let mut req = Some(req);
    let ext = extension.clone();
    rep.write_body_writer(
        extension.as_str(),
        BodyWriter::new(move |s: OutputStream<u8>| -> Future<()> {
            let file_name = file_name.clone();
            // The body writer is called once, so the request is still there.
            let os = match req.take() {
                Some(req) => transformed_stream(transformer.as_ref(), req, &ext, s),
                None => s,
            };
            do_with(os, move |os: &mut OutputStream<u8>| {
                let osp: *mut OutputStream<u8> = os;
                open_file_dma(file_name.as_str(), OpenFlags::RO).then(move |f: File| {
                    do_with(
                        make_file_input_stream(f, FileInputStreamOptions::default()),
                        move |is: &mut InputStream<u8>| {
                            let isp: *mut InputStream<u8> = is;
                            // SAFETY: the two do_withs keep both streams alive and in place for
                            // the whole chain, whose steps, their only users, run one at a time.
                            unsafe { copy(&mut *isp, &mut *osp) }
                                // SAFETY: as above.
                                .finally(move || unsafe { (*osp).close() })
                                // SAFETY: as above.
                                .finally(move || unsafe { (*isp).close() })
                        },
                    )
                })
            })
        }),
    );
    make_ready_future(rep)
}

/// A handler that serves the files under a directory.
///
/// It joins the directory given at construction with the request's `path` parameter and
/// returns that file: with the directory `/usr/mgmt/public` and the path parameter
/// `index.html`, it returns `/usr/mgmt/public/index.html`.
pub struct DirectoryHandler {
    /// The `file_interaction_handler` base.
    pub file: FileInteractionHandler,
    doc_root: Sstring,
}

impl DirectoryHandler {
    /// Creates a handler that serves the files under `doc_root`, transformed by
    /// `transformer` if given.
    ///
    /// With the root `/usr/mgmt/public` and the path parameter `/css/style.css`, the file
    /// served is `/usr/mgmt/public/css/style.css`.
    pub fn new(doc_root: &Sstring, transformer: Option<Box<dyn FileTransformer>>) -> Self {
        DirectoryHandler {
            file: FileInteractionHandler::new(transformer),
            doc_root: doc_root.clone(),
        }
    }
}

/// Whether a decoded path could escape the document root: any `..` segment, or a leading
/// `/` (`get_decoded_param()` strips the matcher's own `/`, so one here means a doubled
/// slash like `//etc`). The value is already percent-decoded, so an encoded traversal such as
/// `%2e%2e%2f` is a plain `../` here and is caught too.
fn is_unsafe_path(path: &[u8]) -> bool {
    path.first() == Some(&b'/') || path.split(|&c| c == b'/').any(|segment| segment == b"..")
}

impl HandlerBase for DirectoryHandler {
    fn base(&self) -> &HandlerBaseData {
        &self.file.base
    }

    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.file.base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        req: Box<Request>,
        rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        let decoded_path = req.param.get_decoded_param(&Sstring::from("path"));
        // Refuse a path that could escape the document root before touching the filesystem.
        if is_unsafe_path(decoded_path.as_bytes()) {
            let mut rep = rep;
            rep.set_status(StatusType::NOT_FOUND, Sstring::new());
            return make_ready_future(rep);
        }
        let mut full_path = self.doc_root.clone();
        full_path.append_bytes(decoded_path.as_bytes());
        // The continuation holds the transformer, not the handler, which may go first.
        let transformer = self.file.transformer.clone();
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe {
            (*engine()).file_type(full_path.as_str(), crate::engine::core::api::FollowSymlink::YES)
        }
        .then(move |val: Option<DirectoryEntryType>| {
            let mut rep = rep;
            let mut full_path = full_path;
            if let Some(val) = val {
                if val == DirectoryEntryType::Directory {
                    if redirect_if_needed(&req, &mut rep) {
                        return make_ready_future(rep);
                    }
                    full_path.append_bytes(b"/index.html");
                }
                return read_file(transformer, full_path, req, rep);
            }
            rep.set_status(StatusType::NOT_FOUND, Sstring::new()).done();
            make_ready_future(rep)
        })
    }
}

/// A handler that always returns the content of one file on disk.
pub struct FileHandler {
    /// The `file_interaction_handler` base.
    pub file_base: FileInteractionHandler,
    file: Sstring,
    force_path: bool,
}

impl FileHandler {
    /// Creates a handler that returns the file at the full path `file`, transformed by
    /// `transformer` if given.
    ///
    /// With `force_path`, handling a request first redirects a URL without a trailing
    /// slash (see [`FileInteractionHandler::redirect_if_needed`]).
    pub fn new(
        file: &Sstring,
        transformer: Option<Box<dyn FileTransformer>>,
        force_path: bool,
    ) -> Self {
        FileHandler {
            file_base: FileInteractionHandler::new(transformer),
            file: file.clone(),
            force_path,
        }
    }
}

impl HandlerBase for FileHandler {
    fn base(&self) -> &HandlerBaseData {
        &self.file_base.base
    }

    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.file_base.base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        if self.force_path && self.file_base.redirect_if_needed(&req, &mut rep) {
            return make_ready_future(rep);
        }
        let file = self.file.clone();
        self.file_base.read(file, req, rep)
    }
}
