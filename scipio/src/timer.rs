// Unless explicitly stated otherwise all files in this repository are licensed under the
// MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//
use crate::parking::Reactor;
use crate::task::JoinHandle;
use crate::{Local, QueueNotFoundError, Task, TaskQueueHandle};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Inner {
    id: u64,

    waker: Option<Waker>,

    /// When this timer fires.
    when: Instant,
}

impl Inner {
    fn reset(&mut self, dur: Duration) {
        if let Some(_) = self.waker.as_ref() {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(self.id);
        }

        // Update the timeout.
        self.when = Instant::now() + dur;

        if let Some(waker) = self.waker.as_mut() {
            // Re-register the timer with the new timeout.
            Reactor::get().insert_timer(self.id, self.when, waker);
        }
    }
}

/// A timer that expires after a duration of time.
///
/// Timers are futures that output the [`Instant`] at which they fired.
/// Note that because of that, Timers always block the current task queue
/// in which they currently execute.
///
/// In most situations you will want to use [`TimerActionOnce`]
///
/// # Examples
///
/// Sleep for 100 milliseconds:
///
/// ```
/// use scipio::{LocalExecutor,Timer};
/// use std::time::Duration;
///
/// async fn sleep(dur: Duration) {
///     Timer::new(dur).await;
/// }
///
/// let ex = LocalExecutor::new(None).expect("failed to create local executor");
///
/// ex.run(async {
///     sleep(Duration::from_millis(100)).await;
/// });
/// ```
/// [`TimerActionOnce`]: struct.TimerActionOnce
#[derive(Debug)]
pub struct Timer {
    inner: Rc<RefCell<Inner>>,
}

impl Timer {
    /// Creates a timer that expires after the given duration of time.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::Timer;
    /// use std::time::Duration;
    ///
    /// Timer::new(Duration::from_millis(100));
    /// ```
    pub fn new(dur: Duration) -> Timer {
        Timer {
            inner: Rc::new(RefCell::new(Inner {
                id: Reactor::get().register_timer(),
                waker: None,
                when: Instant::now() + dur,
            })),
        }
    }

    // Useful in generating repeat timers that have a constant
    // id. Not for external usage.
    fn from_id(id: u64, dur: Duration) -> Timer {
        Timer {
            inner: Rc::new(RefCell::new(Inner {
                id,
                waker: None,
                when: Instant::now() + dur,
            })),
        }
    }

    /// Resets the timer to expire after the new duration of time.
    ///
    /// Note that resetting a timer is different from creating a new timer because
    /// [`reset()`][`Timer::reset()`] does not remove the waker associated with the task that is
    /// polling the timer.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::Timer;
    /// use std::time::Duration;
    ///
    /// let mut t = Timer::new(Duration::from_secs(1));
    /// t.reset(Duration::from_millis(100));
    /// ```
    pub fn reset(&mut self, dur: Duration) {
        let mut inner = self.inner.borrow_mut();
        inner.reset(dur);
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(_) = inner.waker.take() {
            // Deregister the timer from the reactor.
            Reactor::get().remove_timer(inner.id);
        }
    }
}

impl Future for Timer {
    type Output = Instant;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.borrow_mut();

        if Instant::now() >= inner.when {
            // Deregister the timer from the reactor if needed
            Reactor::get().remove_timer(inner.id);
            Poll::Ready(inner.when)
        } else {
            // Register the timer in the reactor.
            Reactor::get().insert_timer(inner.id, inner.when, cx.waker());
            inner.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// The TimerActionOnce struct provides an ergonomic way to fire an action at a
/// later point in time.
///
/// In practice [`Timer`] is hard to use because it will always block the
/// current task queue. This is rarely what one wants.
///
/// The TimerActionOnce creates a timer in the background and executes an action
/// when the timer expires. It also provides a convenient way to cancel a timer.
///
/// [`Timer`]: struct.Timer
#[derive(Debug)]
pub struct TimerActionOnce<T> {
    handle: JoinHandle<T, ()>,
    inner: Rc<RefCell<Inner>>,
}

/// The TimerActionRepeat struct provides an ergonomic way to fire a repeated action at
/// specified intervals, without having to fire new [`TimerActionOnce`] events
///
/// [`TimerActionOnce`]: struct.TimerActionOnce
#[derive(Debug)]
pub struct TimerActionRepeat {
    handle: JoinHandle<(), ()>,
    timer_id: u64,
}

impl<T: 'static> TimerActionOnce<T> {
    /// Creates a [`TimerActionOnce`] that will execute the associated future once after some
    /// time is passed
    ///
    /// # Arguments
    ///
    /// * `when` a [`Duration`] that represents when to execute the action.
    /// * `action` a Future to be executed after `when` is elapsed.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionOnce::do_in(Duration::from_millis(100), async move {
    ///         println!("Executed once");
    ///     });
    ///     action.join().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`Duration`]: https://doc.rust-lang.org/std/time/struct.Duration.html
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    pub fn do_in(when: Duration, action: impl Future<Output = T> + 'static) -> TimerActionOnce<T> {
        Self::do_in_into(when, action, Local::current_task_queue()).unwrap()
    }

    /// Creates a [`TimerActionOnce`] that will execute the associated future once after some
    /// time is passed in a specific Task Queue
    ///
    /// # Arguments
    ///
    /// * `when` a [`Duration`] that represents when to execute the action.
    /// * `action` a Future to be executed after `when` is elapsed.
    /// * `tq` the [`TaskQueueHandle`] for the TaskQueue we want.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce, Local, Latency};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let tq = Local::create_task_queue(1, Latency::NotImportant, "test");
    ///     let action = TimerActionOnce::do_in_into(Duration::from_millis(100), async move {
    ///         println!("Executed once");
    ///     }, tq).unwrap();
    ///     action.join().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`Duration`]: https://doc.rust-lang.org/std/time/struct.Duration.html
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    /// [`TaskQueueHandle`]: struct.TaskQueueHandle
    pub fn do_in_into(
        when: Duration,
        action: impl Future<Output = T> + 'static,
        tq: TaskQueueHandle,
    ) -> Result<TimerActionOnce<T>, QueueNotFoundError> {
        let timer_id = Reactor::get().register_timer();
        let timer = Timer::from_id(timer_id, when);
        let inner = timer.inner.clone();

        let task = Task::local_into(
            async move {
                timer.await;
                action.await
            },
            tq,
        )?;

        Ok(TimerActionOnce {
            handle: task.detach(),
            inner,
        })
    }

    /// Creates a [`TimerActionOnce`] that will execute the associated future once at a specific time
    ///
    /// # Arguments
    ///
    /// * `when` an [`Instant`] that represents when to execute the action.
    /// * `action` a Future to be executed at time `when`.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce};
    /// use std::time::{Instant, Duration};
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let when = Instant::now().checked_add(Duration::from_millis(100)).unwrap();
    ///     let action = TimerActionOnce::do_at(when, async move {
    ///         println!("Executed once");
    ///     });
    ///     action.join().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`Instant`]: https://doc.rust-lang.org/std/time/struct.Instant.html
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    pub fn do_at(when: Instant, action: impl Future<Output = T> + 'static) -> TimerActionOnce<T> {
        Self::do_at_into(when, action, Local::current_task_queue()).unwrap()
    }

    /// Creates a [`TimerActionOnce`] that will execute the associated future once at a specific time
    /// in a specific Task Queue.
    ///
    /// # Arguments
    ///
    /// * `when` an [`Instant`] that represents when to execute the action.
    /// * `action` a Future to be executed at time `when`.
    /// * `tq` the [`TaskQueueHandle`] for the TaskQueue we want.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce, Local, Latency};
    /// use std::time::{Instant, Duration};
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let tq = Local::create_task_queue(1, Latency::NotImportant, "test");
    ///     let when = Instant::now().checked_add(Duration::from_millis(100)).unwrap();
    ///     let action = TimerActionOnce::do_at_into(when, async move {
    ///         println!("Executed once");
    ///     }, tq).unwrap();
    ///     action.join().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`Instant`]: https://doc.rust-lang.org/std/time/struct.Instant.html
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    /// [`TaskQueueHandle`]: struct.TaskQueueHandle
    pub fn do_at_into(
        when: Instant,
        action: impl Future<Output = T> + 'static,
        tq: TaskQueueHandle,
    ) -> Result<TimerActionOnce<T>, QueueNotFoundError> {
        let now = Instant::now();
        let dur = {
            if when > now {
                when.duration_since(now)
            } else {
                Duration::from_micros(0)
            }
        };
        Self::do_in_into(dur, action, tq)
    }

    /// Cancel an existing [`TimerActionOnce`] and waits for it to return
    ///
    /// If you want to cancel the timer but doesn't want to .await on it,
    /// prefer [`destroy`].
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionOnce::do_in(Duration::from_millis(100), async move {
    ///         println!("Will not execute this");
    ///     });
    ///     action.cancel().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    /// [`destroy`]: struct.TimerActionOnce.html#method.destroy
    pub async fn cancel(self) {
        self.destroy();
        self.join().await;
    }

    /// Cancel an existing [`TimerActionOnce`], without waiting for it to return
    ///
    /// This is a non-async version of [`cancel`]. It will remove the timer if
    /// it hasn't fired already and destroy the [`TimerActionOnce`] releasing the resources
    /// associated with it, but without blocking the current task. It is still possible
    /// to [`join`] the task if needed.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionOnce::do_in(Duration::from_millis(100), async move {
    ///         println!("Will not execute this");
    ///     });
    ///     action.destroy();
    ///     action.join().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    /// [`cancel`]: struct.TimerActionOnce.html#method.cancel
    /// [`join`]: struct.TimerActionOnce.html#method.join
    pub fn destroy(&self) {
        Reactor::get().remove_timer(self.inner.borrow().id);
        self.handle.cancel();
    }

    /// Waits for a [`TimerActionOnce`] to return
    ///
    /// Returns an [`Option`] with value None if the task was canceled and Some if
    /// the action finished successfuly
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionOnce::do_in(Duration::from_millis(100), async move {
    ///         println!("Execute this in 100ms");
    ///     });
    ///     action.join().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    /// [`Option`]: https://doc.rust-lang.org/std/option/enum.Option.html
    pub async fn join(self) -> Option<T> {
        self.handle.await
    }

    /// Rearm a [`TimerActionOnce`], so it fires in the specified [`Duration`] from now
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionOnce::do_in(Duration::from_millis(100), async move {
    ///         println!("hello");
    ///     });
    ///     action.rearm_in(Duration::from_millis(100));
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    /// [`Duration`]: https://doc.rust-lang.org/std/time/struct.Duration.html
    pub fn rearm_in(&self, dur: Duration) {
        let mut inner = self.inner.borrow_mut();
        inner.reset(dur);
    }

    /// Rearm a [`TimerActionOnce`], so it fires at the specified [`Instant`]
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionOnce};
    /// use std::time::{Duration, Instant};
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionOnce::do_in(Duration::from_millis(100), async move {
    ///         println!("hello");
    ///     });
    ///     action.rearm_at(Instant::now());
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionOnce`]: struct.TimerActionOnce
    /// [`Instant`]: https://doc.rust-lang.org/std/time/struct.Instant.html
    pub fn rearm_at(&self, when: Instant) {
        let now = Instant::now();
        let dur = {
            if when > now {
                when.duration_since(now)
            } else {
                Duration::from_micros(0)
            }
        };
        self.rearm_in(dur);
    }
}

impl TimerActionRepeat {
    /// Creates a [`TimerActionRepeat`] that will execute the associated future repeatedly in a specific
    /// Task Queue until returns None
    ///
    /// # Arguments
    ///
    /// * `action_gen` a Future to be executed repeatedly. The Future's return value must be
    /// Option<Duration>. If [`Some`], It will execute again after Duration elapses. If `None`,
    /// it stops.
    /// * `tq` the [`TaskQueueHandle`] for the TaskQueue we want.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use scipio::{LocalExecutor, TimerActionRepeat, Latency, Local};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let tq = Local::create_task_queue(1, Latency::NotImportant, "test");
    ///     let action = TimerActionRepeat::repeat_into(|| async move {
    ///         println!("Execute this!");
    ///         Some(Duration::from_millis(100))
    ///     }, tq).unwrap();
    ///     action.join().await; // this never returns
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`Duration`]: https://doc.rust-lang.org/std/time/struct.Duration.html
    /// [`TimerActionRepeat`]: struct.TimerActionRepeat
    /// [`TaskQueueHandle`]: struct.TaskQueueHandle
    pub fn repeat_into<G, F>(
        action_gen: G,
        tq: TaskQueueHandle,
    ) -> Result<TimerActionRepeat, QueueNotFoundError>
    where
        G: Fn() -> F + 'static,
        F: Future<Output = Option<Duration>> + 'static,
    {
        let timer_id = Reactor::get().register_timer();

        let task = Task::local_into(
            async move {
                loop {
                    if let Some(period) = action_gen().await {
                        Timer::from_id(timer_id, period).await;
                    } else {
                        break;
                    }
                }
            },
            tq,
        )?;

        Ok(TimerActionRepeat {
            handle: task.detach(),
            timer_id: timer_id,
        })
    }

    /// Creates a [`TimerActionRepeat`] that will execute the associated future repeatedly until
    /// it returns None
    ///
    /// # Arguments
    ///
    /// * `action_gen` a Future to be executed repeatedly. The Future's return value must be
    /// Option<Duration>. If [`Some`], It will execute again after Duration elapses. If `None`,
    /// it stops.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use scipio::{LocalExecutor, TimerActionRepeat};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionRepeat::repeat(|| async move {
    ///         println!("Execute this!");
    ///         Some(Duration::from_millis(100))
    ///     });
    ///     action.join().await; // this never returns
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`Duration`]: https://doc.rust-lang.org/std/time/struct.Duration.html
    /// [`TimerActionRepeat`]: struct.TimerActionRepeat
    pub fn repeat<G, F>(action_gen: G) -> TimerActionRepeat
    where
        G: Fn() -> F + 'static,
        F: Future<Output = Option<Duration>> + 'static,
    {
        Self::repeat_into(action_gen, Local::current_task_queue()).unwrap()
    }

    /// Cancel an existing [`TimerActionRepeat`] and waits for it to return
    ///
    /// If you want to cancel the timer but doesn't want to .await on it,
    /// prefer [`destroy`].
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionRepeat};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionRepeat::repeat(|| async move {
    ///         Some(Duration::from_millis(100))
    ///     });
    ///     action.cancel().await;
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionRepeat`]: struct.TimerActionRepeat
    /// [`destroy`]: struct.TimerActionRepeat.html#method.destroy
    pub async fn cancel(self) {
        self.destroy();
        self.join().await;
    }

    /// Cancel an existing [`TimerActionRepeat`], without waiting for it to return
    ///
    /// This is a non-async version of [`cancel`]. It will remove the timer if
    /// it hasn't fired already and destroy the [`TimerActionRepeat`] releasing the resources
    /// associated with it, but without blocking the current task. It is still possible
    /// to [`join`] the task if needed.
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionRepeat};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionRepeat::repeat(|| async move {
    ///         Some(Duration::from_millis(100))
    ///     });
    ///     action.destroy();
    ///     let v = action.join().await;
    ///     assert!(v.is_none())
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionRepeat`]: struct.TimerActionRepeat
    /// [`cancel`]: struct.TimerActionRepeat.html#method.cancel
    /// [`join`]: struct.TimerActionRepeat.html#method.join
    pub fn destroy(&self) {
        Reactor::get().remove_timer(self.timer_id);
        self.handle.cancel();
    }

    /// Waits for a [`TimerActionRepeat`] to return
    ///
    /// Returns an [`Option`] with value None if the task was canceled and Some(()) if
    /// the action finished successfuly
    ///
    /// # Examples
    ///
    /// ```
    /// use scipio::{LocalExecutor, TimerActionRepeat};
    /// use std::time::Duration;
    ///
    /// let handle = LocalExecutor::spawn_executor("test", None, || async move {
    ///     let action = TimerActionRepeat::repeat(|| async move {
    ///         None
    ///     });
    ///     let v = action.join().await;
    ///     assert!(v.is_some())
    /// }).unwrap();
    /// handle.join().unwrap();
    /// ```
    /// [`TimerActionRepeat`]: struct.TimerActionRepeat
    /// [`Option`]: https://doc.rust-lang.org/std/option/enum.Option.html
    pub async fn join(self) -> Option<()> {
        self.handle.await.and_then(|_| Some(()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[test]
    fn basic_timer_works() {
        test_executor!(async move {
            let now = Instant::now();
            Timer::new(Duration::from_millis(100)).await;
            assert!(now.elapsed().as_millis() >= 100)
        });
    }

    #[test]
    fn basic_timer_action_instant_works() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let when = Instant::now()
                .checked_add(Duration::from_millis(50))
                .unwrap();
            let _ = TimerActionOnce::do_at(when, async move {
                *(exec1.borrow_mut()) = 1;
            });

            Timer::new(Duration::from_millis(100)).await;
            assert_eq!(*(exec2.borrow()), 1);
        });
    }

    #[test]
    fn basic_timer_action_instant_past_works() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let when = Instant::now()
                .checked_sub(Duration::from_millis(50))
                .unwrap();
            let _ = TimerActionOnce::do_at(when, async move {
                *(exec1.borrow_mut()) = 1;
            });

            Task::<()>::later().await;
            assert_eq!(*(exec2.borrow()), 1);
        });
    }

    #[test]
    fn basic_timer_action_works() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let _ = TimerActionOnce::do_in(Duration::from_millis(50), async move {
                *(exec1.borrow_mut()) = 1;
            });

            Timer::new(Duration::from_millis(100)).await;
            assert_eq!(*(exec2.borrow()), 1);
        });
    }

    #[test]
    fn basic_timer_rearm_pending_timer_for_the_past_ok() {
        test_executor!(async move {
            let now = Instant::now();
            let action: TimerActionOnce<usize> =
                TimerActionOnce::do_in(Duration::from_millis(50), async move {
                    Timer::new(Duration::from_millis(50)).await;
                    1
                });

            Timer::new(Duration::from_millis(60)).await;
            action.rearm_at(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
            let ret = action.join().await;
            assert_eq!(ret.unwrap(), 1);
            assert!(now.elapsed().as_millis() >= 100);
        });
    }

    #[test]
    fn basic_timer_rearm_executed_action_ok() {
        test_executor!(async move {
            let action: TimerActionOnce<usize> =
                TimerActionOnce::do_in(Duration::from_millis(1), async move { 1 });

            Timer::new(Duration::from_millis(10)).await;
            action.rearm_at(
                Instant::now()
                    .checked_add(Duration::from_secs(100))
                    .unwrap(),
            );
            let now = Instant::now();
            let ret = action.join().await;
            assert_eq!(ret.unwrap(), 1);
            assert!(now.elapsed().as_millis() <= 10);
        });
    }

    #[test]
    fn basic_timer_rearm_future_timer_ok() {
        test_executor!(async move {
            let now = Instant::now();
            let action: TimerActionOnce<usize> =
                TimerActionOnce::do_in(Duration::from_millis(10), async move { 1 });

            action.rearm_in(Duration::from_millis(100));
            let ret = action.join().await;
            assert_eq!(ret.unwrap(), 1);
            assert!(now.elapsed().as_millis() >= 100);
        });
    }

    #[test]
    fn basic_timer_action_return_ok() {
        test_executor!(async move {
            let now = Instant::now();
            let action: TimerActionOnce<usize> =
                TimerActionOnce::do_in(Duration::from_millis(50), async move { 1 });

            let ret = action.join().await;
            assert_eq!(ret.unwrap(), 1);
            assert!(now.elapsed().as_millis() >= 50);
        });
    }

    #[test]
    fn basic_timer_action_join_reflects_cancel() {
        test_executor!(async move {
            let now = Instant::now();
            let action: TimerActionOnce<usize> =
                TimerActionOnce::do_in(Duration::from_millis(50), async move { 1 });

            action.destroy();
            let ret = action.join().await;
            assert!(ret.is_none());
            assert!(now.elapsed().as_millis() < 50);
        });
    }

    #[test]
    fn basic_timer_action_cancel_works() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let action = TimerActionOnce::do_in(Duration::from_millis(50), async move {
                *(exec1.borrow_mut()) = 1;
            });
            // Force this to go into the task queue to make the test more
            // realistic
            Task::<()>::later().await;
            action.cancel().await;

            Timer::new(Duration::from_millis(100)).await;
            assert_eq!(*(exec2.borrow()), 0);
        });
    }

    #[test]
    fn basic_timer_action_destroy_works() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let action = TimerActionOnce::do_in(Duration::from_millis(50), async move {
                *(exec1.borrow_mut()) = 1;
            });
            action.destroy();

            Timer::new(Duration::from_millis(100)).await;
            assert_eq!(*(exec2.borrow()), 0);
            // joining doesn't lead to infinite blocking or anything, and eventually completes.
            action.join().await;
        });
    }

    #[test]
    fn basic_timer_action_destroy_cancel_initiated_action() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let action = TimerActionOnce::do_in(Duration::from_millis(10), async move {
                *(exec1.borrow_mut()) = 1;
                // Test that if we had already started the action, it will run to completion.
                for _ in 0..10 {
                    Timer::new(Duration::from_millis(10)).await;
                    *(exec1.borrow_mut()) += 1;
                }
            });
            Timer::new(Duration::from_millis(50)).await;
            action.destroy();

            action.join().await;
            // it did start, but should not have finished
            assert!(*(exec2.borrow()) > 1);
            assert_ne!(*(exec2.borrow()), 11);
        });
    }

    #[test]
    fn basic_timer_action_destroy_detached_spawn_survives() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let action = TimerActionOnce::do_in(Duration::from_millis(10), async move {
                Local::local(async move {
                    *(exec1.borrow_mut()) = 1;
                    // Test that if we had already started the action, it will run to completion.
                    for _ in 0..10 {
                        Timer::new(Duration::from_millis(10)).await;
                        *(exec1.borrow_mut()) += 1;
                    }
                })
                .detach();
            });

            Timer::new(Duration::from_millis(50)).await;
            action.destroy();
            action.join().await;
            // When action completes we are halfway through the count
            assert_ne!(*(exec2.borrow()), 11);
            Timer::new(Duration::from_millis(100)).await;

            // But because it is detached then it completes the count
            assert_eq!(*(exec2.borrow()), 11);
        });
    }

    #[test]
    fn basic_timer_action_cancel_fails_if_fired() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let action = TimerActionOnce::do_in(Duration::from_millis(1), async move {
                *(exec1.borrow_mut()) = 1;
            });
            // Force this to go into the task queue to make the test more
            // realistic
            Timer::new(Duration::from_millis(10)).await;
            action.cancel().await;

            Timer::new(Duration::from_millis(90)).await;
            // too late, fired
            assert_eq!(*(exec2.borrow()), 1);
        });
    }

    #[test]
    fn basic_timer_action_repeat_works() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let repeat = TimerActionRepeat::repeat(move || {
                let ex = exec1.clone();
                async move {
                    *(ex.borrow_mut()) += 1;
                    if (*ex.borrow()) == 10 {
                        return None;
                    } else {
                        return Some(Duration::from_millis(5));
                    }
                }
            });
            Timer::new(Duration::from_millis(100)).await;
            let value = *(exec2.borrow());
            assert!(value == 10);
            let v = repeat.join().await;
            assert!(v.is_some());
        });
    }

    #[test]
    fn basic_timer_action_repeat_cancellation_works() {
        make_shared_var_mut!(0, exec1, exec2);

        test_executor!(async move {
            let action = TimerActionRepeat::repeat(move || {
                let ex = exec1.clone();
                async move {
                    *(ex.borrow_mut()) += 1;
                    Some(Duration::from_millis(10))
                }
            });
            Timer::new(Duration::from_millis(50)).await;
            action.cancel().await;
            let old_value = *(exec2.borrow());
            Timer::new(Duration::from_millis(50)).await;
            assert_eq!(*(exec2.borrow()), old_value);
        });
    }

    #[test]
    fn basic_timer_action_repeat_destruction_works() {
        test_executor!(async move {
            let action =
                TimerActionRepeat::repeat(move || async move { Some(Duration::from_millis(10)) });
            action.destroy();
            let v = action.join().await;
            assert!(v.is_none());
        });
    }
}
