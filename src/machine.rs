use std::mem;

use futures::{Async, Future, Poll};

pub enum Turn<S>
where
    S: State,
{
    Continue(S),
    Suspend(S),
    Done(S::Final),
}

pub trait State: Sized {
    type Final;

    type Item;

    type Error;

    type Context;

    fn turn(state: Self, context: &mut Self::Context) -> Result<Turn<Self>, Self::Error>;

    fn finalize(f: Self::Final, context: Self::Context) -> Result<Self::Item, Self::Error>;
}

pub struct Machine<S>
where
    S: State,
{
    state: Option<S>,
    context: Option<S::Context>,
}

impl<S> Machine<S>
where
    S: State,
{
    pub fn new(state: S, context: S::Context) -> Self {
        Self {
            state: Some(state),
            context: Some(context),
        }
    }
}

impl<S> Future for Machine<S>
where
    S: State,
{
    type Item = S::Item;

    type Error = S::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut state = mem::replace(&mut self.state, None).unwrap();
        loop {
            let turn = S::turn(state, self.context.as_mut().unwrap())?;

            match turn {
                Turn::Continue(next_state) => {
                    state = next_state;
                }
                Turn::Suspend(next_state) => {
                    self.state = Some(next_state);
                    return Ok(Async::NotReady);
                }
                Turn::Done(fin) => {
                    let context = self.context.take().unwrap();
                    return S::finalize(fin, context).map(Async::Ready);
                }
            }
        }
    }
}
