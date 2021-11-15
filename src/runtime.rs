extern crate alloc;
use crate::context::Context as ExecutorContext;
use crate::executor::Executor;
use crate::waker_page::{WakerPage, WakerPageRef, WAKER_PAGE_SIZE};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use log::warn;
use core::cell::RefCell;
use lazy_static::*;
use spin::Mutex;
use unicycle::pin_slab::PinSlab;
use {
    alloc::boxed::Box,
    core::cell::RefMut,
    core::future::Future,
    core::pin::Pin,
    core::task::{Context, Poll},
};

pub enum TaskState {
    _BLOCKED,
    RUNNABLE,
    _RUNNING,
}

pub struct Task {
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    state: Mutex<TaskState>,
    _priority: u8,
}

impl Future for Task {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut f = self.future.lock();
        return f.as_mut().poll(cx);
    }
}

impl Task {
    pub fn _is_runnable(&self) -> bool {
        let task_state = self.state.lock();
        if let TaskState::RUNNABLE = *task_state {
            true
        } else {
            false
        }
    }

    pub fn _block(&self) {
        let mut task_state = self.state.lock();
        *task_state = TaskState::_BLOCKED;
    }
}

pub struct Inner<F: Future<Output = ()> + Unpin> {
    pub slab: PinSlab<F>,
    // root_waker: SharedWaker,
    pub pages: Vec<WakerPageRef>,
}

impl<F: Future<Output = ()> + Unpin> Inner<F> {
    /// Our pages hold 64 contiguous future wakers, so we can do simple arithmetic to access the
    /// correct page as well as the index within page.
    /// Given the `key` representing a future, return a reference to that page, `WakerPageRef`. And
    /// the index _within_ that page (usize).
    pub fn page(&self, key: u64) -> (&WakerPageRef, usize) {
        let key = key as usize;
        let (page_ix, subpage_ix) = (key / WAKER_PAGE_SIZE, key % WAKER_PAGE_SIZE);
        (&self.pages[page_ix], subpage_ix)
    }

    /// Insert a future into our scheduler returning an integer key representing this future. This
    /// key is used to index into the slab for accessing the future.
    pub fn insert(&mut self, future: F) -> u64 {
        let key = self.slab.insert(future);

        // Add a new page to hold this future's status if the current page is filled.
        while key >= self.pages.len() * WAKER_PAGE_SIZE {
            self.pages.push(WakerPage::new());
        }
        let (page, subpage_ix) = self.page(key as u64);
        page.initialize(subpage_ix);
        key as u64
    }
}

const DEFAULT_PRIORITY: usize = 4;
const MAX_PRIORITY: usize = 32;

pub struct PriorityInner<F: Future<Output = ()> + Unpin> {
    inners: Vec<RefCell<Inner<F>>>,
}

impl<F: Future<Output = ()> + Unpin> PriorityInner<F> {
    pub fn new() -> Self {
        let mut inner_vec = PriorityInner { inners: vec![] };
        for _ in 0..MAX_PRIORITY {
            let inner = Inner {
                slab: PinSlab::new(),
                pages: vec![],
            };
            inner_vec.inners.push(RefCell::new(inner));
        }
        return inner_vec;
    }

    fn page(&self, priority: usize, key: u64) -> (&WakerPageRef, usize) {
        let key = key as usize;
        let (page_ix, subpage_ix) = (key / WAKER_PAGE_SIZE, key % WAKER_PAGE_SIZE);
        let ptr = self.inners[priority as usize].as_ptr();
        unsafe { (&((*ptr).pages[page_ix]), subpage_ix) }
    }

    // 插入一个Future, 其优先级为 DEFAULT_PRIORITY
    fn insert(&self, future: F) {
        self.priority_insert(DEFAULT_PRIORITY, future);
    }

    fn priority_insert(&self, priority: usize, future: F) -> u64 {
        log::warn!("priority insert idx={} sz={}", priority, self.inners.len());
        return self.inners[priority].borrow_mut().insert(future);
    }

    pub fn get_mut_inner(&self, priority: usize) -> RefMut<'_, Inner<F>> {
        return self.inners[priority].borrow_mut();
    }
}

pub struct ExecutorRuntime<F: Future<Output = ()> + Unpin> {
    // 只会在一个core上运行，不需要考虑同步问题
    priority_inner: Arc<PriorityInner<F>>,

    // 通过force_switch_future会将strong_executor降级为weak_executor
    strong_executor: Pin<Box<Executor<F>>>,

    // 该executor在执行完一次后就会被drop
    weak_executor: Option<Box<Executor<F>>>,
}

impl<F: Future<Output = ()> + Unpin> ExecutorRuntime<F> {
    pub fn new() -> Self {
        let priority_inner = Arc::new(PriorityInner::new());
        let priority_inner_cloned = priority_inner.clone();
        let e = ExecutorRuntime {
            priority_inner: priority_inner,
            strong_executor: Executor::new(priority_inner_cloned),
            weak_executor: None,
        };
        e
    }

    pub fn run(&self) {
        self.strong_executor.run();
    }

    // 添加一个task，它的初始状态是notified，也就是说它可以被执行.
    fn add_task(&self, priority: usize, future: F) -> u64 {
        assert!(priority < MAX_PRIORITY);
        let mut inner = self.priority_inner.get_mut_inner(priority);
        let key = inner.insert(future);
        let (_page, _) = inner.page(key);
        key
    }
}

// 运行executor.run()
#[no_mangle]
pub(crate) fn run_executor(executor_addr: usize) {
    log::warn!("executor addr {:x}", executor_addr);
    unsafe {
        let p = Box::from_raw(executor_addr as *mut Executor<Task>);
        p.run();
    }
}

unsafe impl Send for ExecutorRuntime<Task> {}
unsafe impl Sync for ExecutorRuntime<Task> {}

lazy_static! {
    pub static ref GLOBAL_RUNTIME: ExecutorRuntime<Task> = ExecutorRuntime::new();
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    log::warn!("in spawn");
    return priority_spawn(future, DEFAULT_PRIORITY);
}

pub fn run() {
    // GLOBAL_RUNTIME.run();
    log::warn!("GLOBAL_RUNTIME.run()");
    let cx = ExecutorContext::default();
    unsafe {
        crate::switch(
            &cx as *const ExecutorContext as usize,
            &(GLOBAL_RUNTIME.strong_executor.context) as *const _ as usize,
        );
    }
}

pub fn priority_spawn(future: impl Future<Output = ()> + Send + 'static, priority: usize) {
    log::warn!("in priority_spawn");
    let bf: Pin<alloc::boxed::Box<dyn Future<Output = ()> + Send + 'static>> = Box::pin(future);
    let future = Mutex::from(bf);
    let state = Mutex::from(TaskState::RUNNABLE);
    let task = Task {
        future,
        state,
        _priority: priority as u8,
    };
    GLOBAL_RUNTIME.add_task(priority, task);
}

pub fn force_switch_future() {}
