//! API documentation served by the HTTP server (swagger 1.2 and 2.0 JSON).
//!
//! An [`ApiRegistryBuilder`] or [`ApiRegistryBuilder20`] installs, at a base path of the
//! routing table, a registry handler that serves the API listing; later calls register
//! further API files or generator functions ([`DocEntry`]) with that same handler, found
//! again through the routes. The documents are streamed to the reply's output stream.
//!
//! The JSON objects name their members at construction and register them with a
//! [`JsonBase`] on demand, in a fixed order, so they hold no self-references and stay
//! valid when the object moves (inside a JSON list too).

use crate::engine::core::do_with::do_with;
use crate::engine::core::file::{File, OpenFlags, open_file_dma};
use crate::engine::core::fstream::{FileInputStreamOptions, make_file_input_stream};
use crate::engine::core::future::{ExceptionPtr, Future, make_exception_future, make_ready_future};
use std::cell::RefCell;
use std::rc::Rc;

use crate::engine::core::iostream::internal::StreamCopyConsumer;
use crate::engine::core::iostream::{InputStream, OutputStream, OutputStreamState};
use crate::engine::core::loop_::do_for_each;
use crate::engine::core::sstring::Sstring;
use crate::engine::http::common::httpd::OperationType;
use crate::engine::http::file_handler::{FileHandler, FileTransformer};
use crate::engine::http::handlers::{HandlerBase, HandlerBaseData, downcast_handler};
use crate::engine::http::reply::{BodyWriter, Reply};
use crate::engine::http::request::Request;
use crate::engine::http::routes::Routes;
use crate::engine::http::transformers::ContentReplace;
use crate::engine::json::formatter::{JsonWrite, ToJson};
use crate::engine::json::json_elements::{
    JsonBase, JsonBaseElement, JsonElement, JsonList, Jsonable, json_base_add,
};

/// A documented api: its path and its description.
#[derive(Clone)]
pub struct ApiDoc {
    /// The path.
    pub path: JsonElement<Sstring>,
    /// The description.
    pub description: JsonElement<Sstring>,
}

impl Default for ApiDoc {
    /// The constructor: `register_params()` names the members.
    fn default() -> Self {
        let mut d = ApiDoc { path: JsonElement::default(), description: JsonElement::default() };
        d.register_params();
        d
    }
}

impl ApiDoc {
    fn register_params(&mut self) {
        json_base_add(&mut self.path, "path", false);
        json_base_add(&mut self.description, "description", false);
    }

    /// Copies the path and the description of `e` into `self`.
    pub fn assign_from(&mut self, e: &ApiDoc) -> &mut Self {
        self.path = e.path.clone();
        self.description = e.description.clone();
        self
    }
}

impl JsonBase for ApiDoc {
    fn element_count(&self) -> usize {
        2
    }

    fn element(&self, i: usize) -> Option<&dyn JsonBaseElement> {
        match i {
            0 => Some(&self.path),
            1 => Some(&self.description),
            _ => None,
        }
    }
}

impl Jsonable for ApiDoc {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        self.base_to_json()
    }

    fn write(self: Box<Self>, s: &mut OutputStreamState<u8>) -> Future<()> {
        (*self).base_write(s)
    }
}

impl ToJson for ApiDoc {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Jsonable::to_json(self)
    }
}

impl JsonWrite for ApiDoc {
    fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
        self.base_write(s)
    }
}

/// The documentation of the apis of a server.
#[derive(Clone)]
pub struct ApiDocs {
    /// The api version.
    pub api_version: JsonElement<Sstring>,
    /// The swagger version.
    pub swagger_version: JsonElement<Sstring>,
    /// The apis.
    pub apis: JsonList<ApiDoc>,
}

impl Default for ApiDocs {
    fn default() -> Self {
        let mut d = ApiDocs {
            api_version: JsonElement::default(),
            swagger_version: JsonElement::default(),
            apis: JsonList::default(),
        };
        d.register_params();
        d.api_version.set(Sstring::from("0.0.1"));
        d.swagger_version.set(Sstring::from("1.2"));
        d
    }
}

impl ApiDocs {
    fn register_params(&mut self) {
        json_base_add(&mut self.api_version, "apiVersion", false);
        json_base_add(&mut self.swagger_version, "swaggerVersion", false);
        json_base_add(&mut self.apis, "apis", false);
    }

    /// Copies the apis of `e` into `self`.
    pub fn assign_from(&mut self, e: &ApiDocs) -> &mut Self {
        self.apis = e.apis.clone();
        self
    }
}

impl JsonBase for ApiDocs {
    fn element_count(&self) -> usize {
        3
    }

    fn element(&self, i: usize) -> Option<&dyn JsonBaseElement> {
        match i {
            0 => Some(&self.api_version),
            1 => Some(&self.swagger_version),
            2 => Some(&self.apis),
            _ => None,
        }
    }
}

impl Jsonable for ApiDocs {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        self.base_to_json()
    }

    fn write(self: Box<Self>, s: &mut OutputStreamState<u8>) -> Future<()> {
        (*self).base_write(s)
    }
}

/// The common part of the api registries: a handler registered at the base path.
pub struct ApiRegistryBase {
    base: HandlerBaseData,
    /// The path of the documentation.
    pub _base_path: Sstring,
    /// The directory of the documentation files.
    pub _file_directory: Sstring,
    /// The routes the registry is registered in; they outlive it (they own it).
    pub _routes: *mut Routes,
}

impl ApiRegistryBase {
    /// A registry of `routes`.
    pub fn new(routes: &mut Routes, file_directory: &Sstring, base_path: &Sstring) -> Self {
        ApiRegistryBase {
            base: HandlerBaseData::default(),
            _base_path: base_path.clone(),
            _file_directory: file_directory.clone(),
            _routes: routes,
        }
    }

    /// Registers `h` at the base path; the routes take ownership of it on success, and
    /// on failure it stays with the caller.
    ///
    /// # Safety
    /// `h` comes from `Box::into_raw` and is used by nobody else; the routes outlive the
    /// registry.
    pub unsafe fn set_route(&mut self, h: *mut dyn HandlerBase) -> Result<(), ExceptionPtr> {
        // SAFETY: the caller's contract.
        unsafe { (*self._routes).put_raw(OperationType::Get, self._base_path.as_str(), h, true) }
    }
}

/// The swagger 1.2 registry of api documentation.
pub struct ApiRegistry {
    /// The `api_registry_base` base.
    pub reg_base: ApiRegistryBase,
    _docs: ApiDocs,
}

impl ApiRegistry {
    /// A registry, registered in `routes`, which own it.
    pub fn new(
        routes: &mut Routes,
        file_directory: &Sstring,
        base_path: &Sstring,
    ) -> Result<*mut ApiRegistry, ExceptionPtr> {
        let p = Box::into_raw(Box::new(ApiRegistry {
            reg_base: ApiRegistryBase::new(routes, file_directory, base_path),
            _docs: ApiDocs::default(),
        }));
        // SAFETY: `p` was just leaked by Box::into_raw; the routes take ownership on success.
        match unsafe { (*p).reg_base.set_route(p) } {
            Ok(()) => Ok(p),
            Err(e) => {
                // SAFETY: the routes refused it, so it is still ours.
                unsafe { drop(Box::from_raw(p)) };
                Err(e)
            }
        }
    }

    /// Documents `api` and serves its json file.
    pub fn reg(
        &mut self,
        api: &Sstring,
        description: &Sstring,
        alternative_path: &Sstring,
    ) -> Result<(), ExceptionPtr> {
        let mut doc = ApiDoc::default();
        doc.description.set(description.clone());
        let mut p = Sstring::from("/");
        p.append_bytes(api.as_bytes());
        doc.path.set(p);
        self._docs.apis.push(doc);
        let path = if alternative_path.empty() {
            let mut p = self.reg_base._file_directory.clone();
            p.append_bytes(api.as_bytes());
            p.append_bytes(b".json");
            p
        } else {
            alternative_path.clone()
        };
        let transformer: Box<dyn FileTransformer> =
            Box::new(ContentReplace::new(&Sstring::from("json")));
        let index: Box<dyn HandlerBase> =
            Box::new(FileHandler::new(&path, Some(transformer), true));
        let mut url = self.reg_base._base_path.clone();
        url.append_bytes(b"/");
        url.append_bytes(api.as_bytes());
        // SAFETY: the routes outlive the registry.
        unsafe { (*self.reg_base._routes).put(OperationType::Get, url.as_str(), index) }.map(|_| ())
    }
}

impl HandlerBase for ApiRegistry {
    fn base(&self) -> &HandlerBaseData {
        &self.reg_base.base
    }

    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.reg_base.base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        _req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        match Jsonable::to_json(&self._docs) {
            Ok(c) => rep._content = c,
            Err(e) => return make_exception_future(e),
        }
        rep.done_with("json");
        make_ready_future(rep)
    }
}

/// The default directory of the documentation files.
pub const DEFAULT_DIR: &str = ".";
/// The default path of the documentation.
pub const DEFAULT_PATH: &str = "/api-doc";

/// The common part of the registry builders.
#[derive(Clone, Debug)]
pub struct ApiRegistryBuilderBase {
    /// The directory of the documentation files.
    pub _file_directory: Sstring,
    /// The path of the documentation.
    pub _base_path: Sstring,
}

impl Default for ApiRegistryBuilderBase {
    fn default() -> Self {
        Self::new(&Sstring::from(DEFAULT_DIR), &Sstring::from(DEFAULT_PATH))
    }
}

impl ApiRegistryBuilderBase {
    /// A builder of registries at `base_path` with files in `file_directory`.
    pub fn new(file_directory: &Sstring, base_path: &Sstring) -> Self {
        ApiRegistryBuilderBase {
            _file_directory: file_directory.clone(),
            _base_path: base_path.clone(),
        }
    }
}

/// The builder of a swagger 1.2 registry.
#[derive(Clone, Debug, Default)]
pub struct ApiRegistryBuilder {
    /// The `api_registry_builder_base` base.
    pub b: ApiRegistryBuilderBase,
}

impl ApiRegistryBuilder {
    /// A builder of registries at `base_path` with files in `file_directory`.
    pub fn new(file_directory: &Sstring, base_path: &Sstring) -> Self {
        ApiRegistryBuilder { b: ApiRegistryBuilderBase::new(file_directory, base_path) }
    }

    /// Registers the registry in `r`, which owns it.
    pub fn set_api_doc(&self, r: &mut Routes) -> Result<(), ExceptionPtr> {
        ApiRegistry::new(r, &self.b._file_directory, &self.b._base_path).map(|_| ())
    }

    /// Documents `api` in the registry of `r`.
    pub fn register_function(
        &self,
        r: &mut Routes,
        api: &Sstring,
        description: &Sstring,
        alternative_path: &Sstring,
    ) -> Result<(), ExceptionPtr> {
        if let Some(h) = r.get_exact_match(OperationType::Get, &self.b._base_path) {
            // if a handler is found, it was added there by the api_registry_builder
            // with the set_api_doc method, so we know it's the type
            // (checked with a downcast: another handler at that path is an error).
            // SAFETY: the routes own the handler, alive and not borrowed during these calls.
            let Some(reg) = (unsafe { downcast_handler::<ApiRegistry>(h) }) else {
                return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(format!(
                    "api_registry_builder: the handler of {} is not an api_registry",
                    self.b._base_path
                ))));
            };
            // SAFETY: as above.
            return unsafe { (*reg).reg(api, description, alternative_path) };
        }
        Ok(())
    }
}

/// A part of the swagger 2.0 document, written into the stream.
///
/// The entry gets the document's output stream for the call and returns the future of its
/// write. That future may outlive the call: the stream's own operations hold the stream's
/// state until they resolve, so one write (of a whole part) needs nothing more.
pub struct DocEntry(Box<DocWriter>);

/// The writer of a [`DocEntry`].
type DocWriter = dyn FnMut(&mut OutputStreamState<u8>) -> Future<()>;

impl DocEntry {
    /// An entry that writes its part with `f`.
    pub fn new<F: FnMut(&mut OutputStreamState<u8>) -> Future<()> + 'static>(f: F) -> Self {
        DocEntry(Box::new(f))
    }

    /// Writes the part into `os`.
    pub fn call(&mut self, os: &mut OutputStreamState<u8>) -> Future<()> {
        (self.0)(os)
    }
}

/// Returns a [`DocEntry`] that copies the file `file_name` into the output stream.
pub fn get_file_reader(file_name: Sstring) -> DocEntry {
    DocEntry::new(move |os: &mut OutputStreamState<u8>| -> Future<()> {
        // Counted as a pending operation of the stream until the copy has its own hold.
        let os = os.hold();
        open_file_dma(file_name.as_str(), OpenFlags::RO).then(move |f: File| {
            do_with(
                make_file_input_stream(f, FileInputStreamOptions::default()),
                move |is: &mut InputStream<u8>| {
                    let isp: *mut InputStream<u8> = is;
                    // SAFETY: `os` keeps the output state alive; the consumer takes its own hold
                    // of it before `os` goes, and only reads it here.
                    let consumer = StreamCopyConsumer::new(unsafe { &*os.ptr() });
                    // SAFETY: do_with keeps the input stream alive and in place until the chain
                    // resolves; the copy has finished with it when the close runs.
                    is.consume(consumer).then(move |()| unsafe { (*isp).close() })
                },
            )
        })
    })
}

/// API documentation in the Swagger 2.0 format.
///
/// The result is one JSON document with the Swagger definitions: the concatenation of
/// the [`DocEntry`]s in the order they were registered, with the definitions added
/// under the `definitions` section.
///
/// A typical document, from the first entry, through a path entry, to the closing of
/// the entries and the definitions:
///
/// ```text
/// {
///   "swagger": "2.0",
///   "host": "localhost:10000",
///   "basePath": "/v2",
///   "paths": {
///     "/config/{id}": {
///       "get": {
///         "description": "Return a config value",
///         "operationId": "findConfigId",
///         "produces": [
///           "application/json"
///         ]
///       }
///     }
///   },
///   "definitions": {
///     ...
///   }
/// }
/// ```
///
/// The entries live in one shared, reference-counted part that every write holds until it
/// resolves, so a write in progress never refers to the [`ApiDocs20`] value itself.
#[derive(Default)]
pub struct ApiDocs20 {
    parts: Rc<RefCell<ApiDocs20Parts>>,
}

/// The entries and the transformer of an [`ApiDocs20`].
#[derive(Default)]
struct ApiDocs20Parts {
    _apis: Vec<DocEntry>,
    _transform: ContentReplace,
    _definitions: Vec<DocEntry>,
}

/// Which list of entries of the document.
#[derive(Clone, Copy)]
enum Section {
    Apis,
    Definitions,
}

impl ApiDocs20Parts {
    fn section(&mut self, section: Section) -> &mut Vec<DocEntry> {
        match section {
            Section::Apis => &mut self._apis,
            Section::Definitions => &mut self._definitions,
        }
    }
}

/// Writes the `i`-th entry of `section` into `os`. The entries are borrowed only while the
/// entry is called, so adding an entry while a document is written does not conflict.
fn write_entry(
    parts: &RefCell<ApiDocs20Parts>,
    section: Section,
    i: usize,
    os: &mut OutputStreamState<u8>,
) -> Future<()> {
    let mut parts = parts.borrow_mut();
    match parts.section(section).get_mut(i) {
        Some(entry) => entry.call(os),
        None => make_ready_future(()),
    }
}

/// Writes every entry of `section` into the stream at `os`, in order.
///
/// # Safety
/// `os` points at a stream that stays alive and in place, and unused elsewhere, until the
/// returned future resolves.
unsafe fn write_section(
    parts: Rc<RefCell<ApiDocs20Parts>>,
    section: Section,
    os: *mut OutputStream<u8>,
) -> Future<()> {
    let n = parts.borrow_mut().section(section).len();
    // SAFETY: the contract above; each step borrows the stream only for its call, and the
    // steps run one at a time.
    do_for_each(0..n, move |i| write_entry(&parts, section, i, unsafe { &mut *os }))
}

impl ApiDocs20 {
    /// Writes the document into `os`, replacing the host and the protocol of `req`.
    ///
    /// The returned future owns the stream and holds the entries, so it may outlive
    /// `self`; entries added meanwhile are written if their section has not been reached.
    pub fn write(&self, os: OutputStream<u8>, req: Box<Request>) -> Future<()> {
        let parts = Rc::clone(&self.parts);
        let os = parts.borrow_mut()._transform.transform(req, &Sstring::new(), os);
        do_with(os, move |os: &mut OutputStream<u8>| {
            let osp: *mut OutputStream<u8> = os;
            let definitions = Rc::clone(&parts);
            // SAFETY: do_with keeps the stream alive and in place until the chain resolves,
            // and the steps, the only users of the stream, run one at a time.
            unsafe { write_section(parts, Section::Apis, osp) }
                // SAFETY: as above.
                .then(move |()| unsafe { (*osp).write_str("},\"definitions\": {") })
                // SAFETY: as above.
                .then(move |()| unsafe { write_section(definitions, Section::Definitions, osp) })
                // SAFETY: as above.
                .then(move |()| unsafe { (*osp).write_str("}}") })
                // SAFETY: as above.
                .then(move |()| unsafe { (*osp).flush() })
                // SAFETY: as above.
                .finally(move || unsafe { (*osp).close() })
        })
    }

    /// Adds a part to the paths section.
    pub fn add_api(&mut self, f: DocEntry) {
        self.parts.borrow_mut()._apis.push(f);
    }

    /// Adds a part to the definitions section.
    pub fn add_definition(&mut self, f: DocEntry) {
        self.parts.borrow_mut()._definitions.push(f);
    }

    /// Another handle to the same document, for a write that outlives the borrow of `self`.
    fn share(&self) -> ApiDocs20 {
        ApiDocs20 { parts: Rc::clone(&self.parts) }
    }
}

/// The swagger 2.0 registry of api documentation.
pub struct ApiRegistry20 {
    /// The `api_registry_base` base.
    pub reg_base: ApiRegistryBase,
    _docs: ApiDocs20,
}

impl ApiRegistry20 {
    /// A registry, registered in `routes`, which own it.
    pub fn new(
        routes: &mut Routes,
        file_directory: &Sstring,
        base_path: &Sstring,
    ) -> Result<*mut ApiRegistry20, ExceptionPtr> {
        let p = Box::into_raw(Box::new(ApiRegistry20 {
            reg_base: ApiRegistryBase::new(routes, file_directory, base_path),
            _docs: ApiDocs20::default(),
        }));
        // SAFETY: `p` was just leaked by Box::into_raw; the routes take ownership on success.
        match unsafe { (*p).reg_base.set_route(p) } {
            Ok(()) => Ok(p),
            Err(e) => {
                // SAFETY: the routes refused it, so it is still ours.
                unsafe { drop(Box::from_raw(p)) };
                Err(e)
            }
        }
    }

    /// Adds a part to the paths section.
    pub fn reg(&mut self, f: DocEntry) {
        self._docs.add_api(f);
    }

    /// Adds a part to the definitions section.
    pub fn add_definition(&mut self, f: DocEntry) {
        self._docs.add_definition(f);
    }
}

impl HandlerBase for ApiRegistry20 {
    fn base(&self) -> &HandlerBaseData {
        &self.reg_base.base
    }

    fn base_mut(&mut self) -> &mut HandlerBaseData {
        &mut self.reg_base.base
    }

    fn handle(
        &mut self,
        _path: &Sstring,
        req: Box<Request>,
        mut rep: Box<Reply>,
    ) -> Future<Box<Reply>> {
        // The body writer runs after this call, so it holds the document itself.
        let docs = self._docs.share();
        let mut req = Some(req);
        rep.write_body_writer(
            "json",
            BodyWriter::new(move |os: OutputStream<u8>| -> Future<()> {
                match req.take() {
                    Some(req) => docs.write(os, req),
                    // The reply calls its body writer once; a further call only closes.
                    None => {
                        let mut os = os;
                        os.close()
                    }
                }
            }),
        );
        make_ready_future(rep)
    }
}

/// The builder of a swagger 2.0 registry.
#[derive(Clone, Debug, Default)]
pub struct ApiRegistryBuilder20 {
    /// The `api_registry_builder_base` base.
    pub b: ApiRegistryBuilderBase,
}

impl ApiRegistryBuilder20 {
    /// A builder of registries at `base_path` with files in `file_directory`.
    pub fn new(file_directory: &Sstring, base_path: &Sstring) -> Self {
        ApiRegistryBuilder20 { b: ApiRegistryBuilderBase::new(file_directory, base_path) }
    }

    fn get_register_base(&self, r: &mut Routes) -> Option<*mut ApiRegistry20> {
        // if a handler is found, it was added there by the api_registry_builder
        // with the set_api_doc method, so we know it's the type
        // (checked with a downcast: another handler at that path gives none).
        let h = r.get_exact_match(OperationType::Get, &self.b._base_path)?;
        // SAFETY: the routes own the handler, alive and not borrowed during the call.
        unsafe { downcast_handler::<ApiRegistry20>(h) }
    }

    /// Registers the registry in `r`, which owns it.
    pub fn set_api_doc(&self, r: &mut Routes) -> Result<(), ExceptionPtr> {
        ApiRegistry20::new(r, &self.b._file_directory, &self.b._base_path).map(|_| ())
    }

    /// Registers the [`DocEntry`] `f` in the registry of `r`, after the entries already
    /// registered.
    ///
    /// The entry can copy its definitions from a file or generate them dynamically. Does
    /// nothing if `r` has no API registry.
    pub fn register_function(&self, r: &mut Routes, f: DocEntry) {
        if let Some(h) = self.get_register_base(r) {
            // SAFETY: `get_register_base` checked that the handler is an `ApiRegistry20`;
            // the routes own it, alive and not borrowed during the call.
            unsafe { (*h).reg(f) };
        }
    }

    /// Registers the API `api`, whose description is the file `<directory>/<api>.json`
    /// of the builder's file directory.
    pub fn register_api_file(&self, r: &mut Routes, api: &Sstring) {
        let mut p = self.b._file_directory.clone();
        p.append_bytes(b"/");
        p.append_bytes(api.as_bytes());
        p.append_bytes(b".json");
        self.register_function(r, get_file_reader(p));
    }

    /// Adds the [`DocEntry`] `f` to the definitions section, written after the path
    /// entries. Does nothing if `r` has no API registry.
    pub fn add_definition(&self, r: &mut Routes, f: DocEntry) {
        if let Some(h) = self.get_register_base(r) {
            // SAFETY: as in register_function().
            unsafe { (*h).add_definition(f) };
        }
    }

    /// Adds the definitions in the file `<directory><file>.def.json` of the builder's
    /// file directory.
    pub fn add_definitions_file(&self, r: &mut Routes, file: &Sstring) {
        let mut p = self.b._file_directory.clone();
        p.append_bytes(file.as_bytes());
        p.append_bytes(b".def.json");
        self.add_definition(r, get_file_reader(p));
    }
}
