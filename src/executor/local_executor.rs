use futures_lite::pin;
use std::{
    cell::RefCell,
    future::Future,
    rc::Rc,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use crate::{executor::LOCAL_EX, parking, reactor::Reactor, task::join_handle::JoinHandle};

use super::{
    local_executor_builder::LocalExecutorBuilder,
    placement::Placement,
    queue_manager::QueueManager,
    task_queue::{TaskQueue, TaskQueueHandle},
};

#[derive(Debug)]
pub(crate) struct LocalExecutor {
    pub(crate) id: usize,
    pub(crate) queues: Rc<RefCell<QueueManager>>,
    reactor: Rc<Reactor>,
    parker: parking::Parker,
}

pub(crate) const DEFAULT_RING_SUBMISSION_DEPTH: usize = 128;

impl LocalExecutor {
    pub fn new(cpu_binding: Option<impl IntoIterator<Item = usize>>) -> Self {
        match cpu_binding {
            Some(cpu_set) => bind_to_cpu_set(cpu_set),
            None => {}
        }
        LocalExecutor {
            id: 0,
            queues: Rc::new(RefCell::new(QueueManager::new())),
            parker: parking::Parker::new(),
            reactor: Rc::new(Reactor::new(DEFAULT_RING_SUBMISSION_DEPTH)),
        }
    }

    pub fn init(&mut self) {
        self.add_default_task_queue();
    }
    pub fn default() -> Self {
        let builder = LocalExecutorBuilder::new(Placement::Unbound);
        builder.build()
    }

    pub fn get_reactor(&self) -> Rc<Reactor> {
        self.reactor.clone()
    }

    pub fn add_default_task_queue(&self) {
        self.queues
            .borrow_mut()
            .available_queues
            .insert(0, TaskQueue::new("default"));
    }

    pub fn get_id(&self) -> usize {
        self.id
    }

    fn get_default_queue(&self) -> Option<Rc<RefCell<TaskQueue>>> {
        self.get_queue(TaskQueueHandle { index: 0 })
    }

    pub(crate) fn get_queue(&self, handle: TaskQueueHandle) -> Option<Rc<RefCell<TaskQueue>>> {
        self.queues
            .borrow()
            .available_queues
            .get(&handle.index)
            .cloned()
    }

    pub(crate) fn spawn<T>(&self, future: impl Future<Output = T>) -> JoinHandle<T> {
        let active_executing = self.queues.borrow().active_executing.clone();
        let tq = active_executing
            .clone() // this clone is cheap because we clone an `Option<Rc<_>>`
            .or_else(|| self.get_default_queue())
            .unwrap();
        let tq_executor = tq.borrow().ex.clone();
        tq_executor.spawn_and_schedule(self.id, tq, future)
    }

    /// Runs the executor until the given future completes.
    pub fn run<T>(&self, future: impl Future<Output = T>) -> T {
        assert!(
            !LOCAL_EX.is_set(),
            "There is already an LocalExecutor running on this thread"
        );
        LOCAL_EX.set(self, || {
            let waker = dummy_waker();
            let cx = &mut Context::from_waker(&waker);
            let join_handle = self.spawn(async move { future.await });
            pin!(join_handle);
            loop {
                if let Poll::Ready(t) = join_handle.as_mut().poll(cx) {
                    // can't be canceled, and join handle is None only upon
                    // cancellation or panic. So in case of panic this just propagates
                    return t.unwrap();
                }

                self.parker
                    .poll_io()
                    .expect("Failed to poll io! This is actually pretty bad!");

                // TODO: I/O work
                self.run_task_queues();
            }
        })
    }

    pub(crate) fn spawn_into<T>(
        &self,
        future: impl Future<Output = T>,
        handle: TaskQueueHandle,
    ) -> JoinHandle<T> {
        todo!()
    }

    fn run_task_queues(&self) -> bool {
        let mut ran = false;
        loop {
            // TODO: Check if prempt
            if !self.run_one_task_queue() {
                println!("run_task_queues: no task executed, returning");
                return false;
            } else {
                println!("run_task_queues: Ran is true, loop again");
                ran = true;
            }
        }
        ran
    }

    // Returns true if a task queue is run
    fn run_one_task_queue(&self) -> bool {
        println!("run_one_task_queue called");
        let mut q_manager = self.queues.borrow_mut();
        let size = q_manager.active_queues.len();
        println!("Size is: {}", size);
        let tq = q_manager.active_queues.pop();
        match tq {
            Some(tq) => {
                q_manager.active_executing = Some(tq.clone());
                drop(q_manager);
                loop {
                    // TODO: Break if pre-empted or yielded
                    let tq = tq.borrow_mut();

                    if let Some(task) = tq.get_task() {
                        drop(tq);
                        task.run();
                    } else {
                        println!("No task. Break!");
                        break;
                    }
                }
                let mut tq_ref = tq.borrow_mut();
                tq_ref.reset_active();
                let need_repush = tq_ref.is_active();
                if need_repush {
                    self.queues.borrow_mut().active_queues.push(tq.clone());
                }
                true
            }
            None => {
                println!("no task queue to run");
                false
            }
        }
    }
}

pub(crate) fn bind_to_cpu_set(cpus: impl IntoIterator<Item = usize>) {
    let mut cpuset = nix::sched::CpuSet::new();
    for cpu in cpus {
        cpuset.set(cpu).unwrap();
    }
    let pid = nix::unistd::Pid::from_raw(0);
    nix::sched::sched_setaffinity(pid, &cpuset).unwrap();
}

pub(crate) fn dummy_waker() -> Waker {
    fn raw_waker() -> RawWaker {
        // the pointer is never dereferenced, so null is ok
        RawWaker::new(std::ptr::null::<()>(), vtable())
    }

    fn vtable() -> &'static RawWakerVTable {
        &RawWakerVTable::new(
            |_| raw_waker(),
            |_| {
                println!("Dummy wake");
            },
            |_| {
                println!("Dummy wake_by_ref");
            },
            |_| {
                println!("Dummy drop");
            },
        )
    }

    unsafe { Waker::from_raw(raw_waker()) }
}
