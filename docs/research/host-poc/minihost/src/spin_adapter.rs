//! Spin KV + variables adapters over a pluggable backend trait.
use wasmtime::{Error, Result};
use std::{collections::HashMap, sync::Arc};
use wasmtime::component::{Accessor, FutureReader, HasData, Linker, Resource, ResourceTable, StreamReader};

wasmtime::component::bindgen!({
    path: "wit",
    world: "adapters",
    imports: { default: trappable },
    with: {
        "spin:key-value/key-value.store": Store,
        "fermyon:spin/key-value.store": Store,
    },
});

use fermyon::spin::{key_value as v2kv, variables as v2var};
use spin::key_value::key_value as v3kv;
use spin::variables::variables as v3var;

/// The one trait an embedder implements to plug in a KV backend.
#[async_trait::async_trait]
pub trait KvBackend: Send + Sync {
    async fn get(&self, store: &str, key: &str) -> Result<Option<Vec<u8>>>;
    async fn set(&self, store: &str, key: &str, value: Vec<u8>) -> Result<()>;
    async fn delete(&self, store: &str, key: &str) -> Result<()>;
    async fn keys(&self, store: &str) -> Result<Vec<String>>;
}

/// Per-component state: allowed store labels, resolved variables, backend.
#[derive(Clone)]
pub struct SpinCtx {
    pub kv: Arc<dyn KvBackend>,
    pub stores: Vec<String>,
    pub vars: HashMap<String, String>,
}
pub struct Store(String);
pub struct SpinView<'a> {
    pub ctx: &'a mut SpinCtx,
    pub table: &'a mut ResourceTable,
}
pub struct Spin;
impl HasData for Spin {
    type Data<'a> = SpinView<'a>;
}
pub trait SpinHost: 'static {
    fn spin(&mut self) -> SpinView<'_>;
}

pub fn add_to_linker<T: SpinHost + Send>(l: &mut Linker<T>) -> Result<()> {
    v3kv::add_to_linker::<_, Spin>(l, T::spin)?;
    v3var::add_to_linker::<_, Spin>(l, T::spin)?;
    v2kv::add_to_linker::<_, Spin>(l, T::spin)?;
    v2var::add_to_linker::<_, Spin>(l, T::spin)?;
    Ok(())
}

// ---- shared logic ----
enum E { NoStore, Denied, Other(String) }
impl From<Error> for E { fn from(e: Error) -> Self { E::Other(format!("{e:#}")) } }
impl From<E> for v3kv::Error {
    fn from(e: E) -> Self { match e { E::NoStore => Self::NoSuchStore, E::Denied => Self::AccessDenied, E::Other(s) => Self::Other(s) } }
}
impl From<E> for v2kv::Error {
    fn from(e: E) -> Self { match e { E::NoStore => Self::NoSuchStore, E::Denied => Self::AccessDenied, E::Other(s) => Self::Other(s) } }
}
impl SpinView<'_> {
    fn open(&mut self, label: String) -> Result<Resource<Store>, E> {
        if !self.ctx.stores.contains(&label) { return Err(E::Denied); }
        self.table.push(Store(label)).map_err(|e| E::Other(e.to_string()))
    }
    fn target(&mut self, h: &Resource<Store>) -> Result<(Arc<dyn KvBackend>, String), E> {
        let s = self.table.get(h).map_err(|_| E::NoStore)?;
        Ok((self.ctx.kv.clone(), s.0.clone()))
    }
    fn var(&self, name: &str) -> Result<String, VarErr> {
        if !name.starts_with(|c: char| c.is_ascii_lowercase()) || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(VarErr::Invalid(name.into()));
        }
        self.ctx.vars.get(name).cloned().ok_or_else(|| VarErr::Undefined(name.into()))
    }
}
enum VarErr { Invalid(String), Undefined(String) }
impl From<VarErr> for v3var::Error {
    fn from(e: VarErr) -> Self { match e { VarErr::Invalid(s) => Self::InvalidName(s), VarErr::Undefined(s) => Self::Undefined(s) } }
}
impl From<VarErr> for v2var::Error {
    fn from(e: VarErr) -> Self { match e { VarErr::Invalid(s) => Self::InvalidName(s), VarErr::Undefined(s) => Self::Undefined(s) } }
}

// ---- spin:key-value@3.0.0 (async) ----
impl v3kv::Host for SpinView<'_> {}
impl v3kv::HostStore for SpinView<'_> {
    fn drop(&mut self, s: Resource<Store>) -> Result<()> { self.table.delete(s)?; Ok(()) }
}
impl<T: 'static> v3kv::HostStoreWithStore<T> for Spin {
    async fn open(a: &Accessor<T, Self>, label: String) -> Result<Result<Resource<Store>, v3kv::Error>> {
        Ok(a.with(|mut x| x.get().open(label)).map_err(Into::into))
    }
    async fn get(a: &Accessor<T, Self>, s: Resource<Store>, key: String) -> Result<Result<Option<Vec<u8>>, v3kv::Error>> {
        let (kv, st) = match a.with(|mut x| x.get().target(&s)) { Ok(t) => t, Err(e) => return Ok(Err(e.into())) };
        Ok(kv.get(&st, &key).await.map_err(|e| E::from(e).into()))
    }
    async fn set(a: &Accessor<T, Self>, s: Resource<Store>, key: String, v: Vec<u8>) -> Result<Result<(), v3kv::Error>> {
        let (kv, st) = match a.with(|mut x| x.get().target(&s)) { Ok(t) => t, Err(e) => return Ok(Err(e.into())) };
        Ok(kv.set(&st, &key, v).await.map_err(|e| E::from(e).into()))
    }
    async fn delete(a: &Accessor<T, Self>, s: Resource<Store>, key: String) -> Result<Result<(), v3kv::Error>> {
        let (kv, st) = match a.with(|mut x| x.get().target(&s)) { Ok(t) => t, Err(e) => return Ok(Err(e.into())) };
        Ok(kv.delete(&st, &key).await.map_err(|e| E::from(e).into()))
    }
    async fn exists(a: &Accessor<T, Self>, s: Resource<Store>, key: String) -> Result<Result<bool, v3kv::Error>> {
        let (kv, st) = match a.with(|mut x| x.get().target(&s)) { Ok(t) => t, Err(e) => return Ok(Err(e.into())) };
        Ok(kv.get(&st, &key).await.map(|v| v.is_some()).map_err(|e| E::from(e).into()))
    }
    async fn get_keys(a: &Accessor<T, Self>, s: Resource<Store>) -> Result<(StreamReader<String>, FutureReader<Result<(), v3kv::Error>>)> {
        let (kv, st) = match a.with(|mut x| x.get().target(&s)) { Ok(t) => t, Err(e) => return Ok(fail(a, e.into())?) };
        match kv.keys(&st).await {
            Ok(keys) => a.with(|mut x| Ok((StreamReader::new(&mut x, keys)?, FutureReader::new(&mut x, async { Ok::<_, wasmtime::Error>(Ok(())) })?))),
            Err(e) => fail(a, E::from(e).into()),
        }
    }
}
fn fail<T: 'static>(a: &Accessor<T, Spin>, e: v3kv::Error) -> Result<(StreamReader<String>, FutureReader<Result<(), v3kv::Error>>)> {
    a.with(|mut x| Ok((StreamReader::new(&mut x, Vec::<String>::new())?, FutureReader::new(&mut x, async move { Ok::<_, wasmtime::Error>(Err(e)) })?)))
}

// ---- spin:variables@3.0.0 (async) ----
impl v3var::Host for SpinView<'_> {}
impl<T: 'static> v3var::HostWithStore<T> for Spin {
    async fn get(a: &Accessor<T, Self>, name: String) -> Result<Result<String, v3var::Error>> {
        Ok(a.with(|mut x| x.get().var(&name)).map_err(Into::into))
    }
}

// ---- legacy fermyon:spin/key-value@2.0.0 + variables@2.0.0 (sync; blocks on the async backend) ----
impl v2kv::Host for SpinView<'_> {}
impl v2kv::HostStore for SpinView<'_> {
    fn open(&mut self, label: String) -> Result<Result<Resource<Store>, v2kv::Error>> { Ok(SpinView::open(self, label).map_err(Into::into)) }
    fn get(&mut self, s: Resource<Store>, key: String) -> Result<Result<Option<Vec<u8>>, v2kv::Error>> {
        Ok(self.target(&s).map_err(v2kv::Error::from).and_then(|(kv, st)| block(kv.get(&st, &key))))
    }
    fn set(&mut self, s: Resource<Store>, key: String, v: Vec<u8>) -> Result<Result<(), v2kv::Error>> {
        Ok(self.target(&s).map_err(v2kv::Error::from).and_then(|(kv, st)| block(kv.set(&st, &key, v))))
    }
    fn delete(&mut self, s: Resource<Store>, key: String) -> Result<Result<(), v2kv::Error>> {
        Ok(self.target(&s).map_err(v2kv::Error::from).and_then(|(kv, st)| block(kv.delete(&st, &key))))
    }
    fn exists(&mut self, s: Resource<Store>, key: String) -> Result<Result<bool, v2kv::Error>> {
        Ok(self.target(&s).map_err(v2kv::Error::from).and_then(|(kv, st)| block(kv.get(&st, &key)).map(|v| v.is_some())))
    }
    fn get_keys(&mut self, s: Resource<Store>) -> Result<Result<Vec<String>, v2kv::Error>> {
        Ok(self.target(&s).map_err(v2kv::Error::from).and_then(|(kv, st)| block(kv.keys(&st))))
    }
    fn drop(&mut self, s: Resource<Store>) -> Result<()> { self.table.delete(s)?; Ok(()) }
}
fn block<R>(f: impl Future<Output = Result<R>>) -> Result<R, v2kv::Error> {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f)).map_err(|e| E::from(e).into())
}
impl v2var::Host for SpinView<'_> {
    fn get(&mut self, name: String) -> Result<Result<String, v2var::Error>> { Ok(self.var(&name).map_err(Into::into)) }
}

/// Trivial in-memory backend (for tests / local dev).
#[derive(Default)]
pub struct MemKv(std::sync::Mutex<HashMap<(String, String), Vec<u8>>>);
#[async_trait::async_trait]
impl KvBackend for MemKv {
    async fn get(&self, s: &str, k: &str) -> Result<Option<Vec<u8>>> { Ok(self.0.lock().unwrap().get(&(s.into(), k.into())).cloned()) }
    async fn set(&self, s: &str, k: &str, v: Vec<u8>) -> Result<()> { self.0.lock().unwrap().insert((s.into(), k.into()), v); Ok(()) }
    async fn delete(&self, s: &str, k: &str) -> Result<()> { self.0.lock().unwrap().remove(&(s.into(), k.into())); Ok(()) }
    async fn keys(&self, s: &str) -> Result<Vec<String>> { Ok(self.0.lock().unwrap().keys().filter(|(a, _)| a == s).map(|(_, k)| k.clone()).collect()) }
}
