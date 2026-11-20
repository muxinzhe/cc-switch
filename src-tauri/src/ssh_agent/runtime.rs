use std::{
    any::{Any, TypeId},
    collections::HashMap,
    marker::PhantomData,
    ops::Deref,
    sync::{Arc, Mutex},
};

use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Default)]
pub struct AppHandle {
    state: Arc<Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
    events: Arc<Mutex<Vec<Value>>>,
}

pub struct State<'a, T>(Arc<T>, PhantomData<&'a T>);

impl<T> State<'_, T> {
    pub fn inner(&self) -> &T {
        self
    }
}

impl<T> Deref for State<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub trait Manager {
    fn try_state<T: Send + Sync + 'static>(&self) -> Option<State<'static, T>>;

    fn state<T: Send + Sync + 'static>(&self) -> State<'static, T> {
        self.try_state().expect("Managed state is not registered")
    }
}

impl Manager for AppHandle {
    fn try_state<T: Send + Sync + 'static>(&self) -> Option<State<'static, T>> {
        let value = self.state.lock().ok()?.get(&TypeId::of::<T>())?.clone();
        Some(State(value.downcast().ok()?, PhantomData))
    }
}

impl AppHandle {
    pub fn manage<T: Send + Sync + 'static>(&self, state: T) {
        self.state
            .lock()
            .unwrap()
            .insert(TypeId::of::<T>(), Arc::new(state));
    }

    pub fn take_events(&self) -> Vec<Value> {
        std::mem::take(&mut self.events.lock().unwrap())
    }
}

pub trait Emitter {
    fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), serde_json::Error>;
}

impl Emitter for AppHandle {
    fn emit<S: Serialize>(&self, event: &str, payload: S) -> Result<(), serde_json::Error> {
        let payload = serde_json::to_value(payload)?;
        let mut events = self.events.lock().unwrap();
        if events.len() >= 256 {
            events.remove(0);
        }
        events.push(json!({ "event": event, "payload": payload }));
        Ok(())
    }
}

pub mod async_runtime {
    use std::{future::Future, sync::OnceLock};
    use tokio::{runtime::Runtime, task::JoinHandle};

    fn runtime() -> &'static Runtime {
        static RUNTIME: OnceLock<Runtime> = OnceLock::new();
        RUNTIME.get_or_init(|| Runtime::new().expect("Could not create the SSH agent runtime"))
    }

    pub fn block_on<F: Future>(future: F) -> F::Output {
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| runtime().block_on(future))
        } else {
            runtime().block_on(future)
        }
    }

    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        runtime().spawn(future)
    }

    pub fn spawn_blocking<F, R>(task: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        runtime().spawn_blocking(task)
    }

    pub fn handle() -> tokio::runtime::Handle {
        runtime().handle().clone()
    }
}
