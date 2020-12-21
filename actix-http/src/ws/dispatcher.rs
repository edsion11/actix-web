#![allow(type_alias_bounds)]

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, mem};

use actix_codec::{AsyncRead, AsyncWrite, Decoder, Encoder, Framed};
use actix_service::{IntoService, Service};
use actix_utils::mpsc;
use futures_core::stream::Stream;
use log::debug;

use super::{Codec, Frame, Message};

#[pin_project::pin_project]
pub struct Dispatcher<S, T>
where
    S: Service<Request = Frame, Response = Message> + 'static,
    T: AsyncRead + AsyncWrite,
{
    #[pin]
    inner: FramedDispatcher<S, T, Codec, Message>,
}

impl<S, T> Dispatcher<S, T>
where
    T: AsyncRead + AsyncWrite,
    S: Service<Request = Frame, Response = Message>,
    S::Future: 'static,
    S::Error: 'static,
{
    pub fn new<F: IntoService<S>>(io: T, service: F) -> Self {
        Dispatcher {
            inner: FramedDispatcher::new(Framed::new(io, Codec::new()), service),
        }
    }

    pub fn with<F: IntoService<S>>(framed: Framed<T, Codec>, service: F) -> Self {
        Dispatcher {
            inner: FramedDispatcher::new(framed, service),
        }
    }
}

impl<S, T> Future for Dispatcher<S, T>
where
    T: AsyncRead + AsyncWrite,
    S: Service<Request = Frame, Response = Message>,
    S::Future: 'static,
    S::Error: 'static,
{
    type Output = Result<(), FramedDispatcherError<S::Error, Codec, Message>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().inner.poll(cx)
    }
}

/// Framed transport errors
pub enum FramedDispatcherError<E, U: Encoder<I> + Decoder, I> {
    Service(E),
    Encoder(<U as Encoder<I>>::Error),
    Decoder(<U as Decoder>::Error),
}

impl<E, U: Encoder<I> + Decoder, I> From<E> for FramedDispatcherError<E, U, I> {
    fn from(err: E) -> Self {
        FramedDispatcherError::Service(err)
    }
}

impl<E, U: Encoder<I> + Decoder, I> fmt::Debug for FramedDispatcherError<E, U, I>
where
    E: fmt::Debug,
    <U as Encoder<I>>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FramedDispatcherError::Service(ref e) => {
                write!(fmt, "DispatcherError::Service({:?})", e)
            }
            FramedDispatcherError::Encoder(ref e) => {
                write!(fmt, "DispatcherError::Encoder({:?})", e)
            }
            FramedDispatcherError::Decoder(ref e) => {
                write!(fmt, "DispatcherError::Decoder({:?})", e)
            }
        }
    }
}

impl<E, U: Encoder<I> + Decoder, I> fmt::Display for FramedDispatcherError<E, U, I>
where
    E: fmt::Display,
    <U as Encoder<I>>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FramedDispatcherError::Service(ref e) => write!(fmt, "{}", e),
            FramedDispatcherError::Encoder(ref e) => write!(fmt, "{:?}", e),
            FramedDispatcherError::Decoder(ref e) => write!(fmt, "{:?}", e),
        }
    }
}

#[allow(dead_code)]
pub enum InnerMessage<T> {
    Item(T),
    // TODO: Remove unused variant from InnerMessage
    Close,
}

#[doc(hidden)]
#[pin_project::pin_project]
/// Dispatcher is a future that reads frames from Framed object
/// and passes them to the service.
pub struct FramedDispatcher<S, T, U, I>
where
    S: Service<Request = <U as Decoder>::Item, Response = I>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead,
    T: AsyncWrite,
    U: Encoder<I>,
    U: Decoder,
    I: 'static,
    <U as Encoder<I>>::Error: fmt::Debug,
{
    service: S,
    state: State<S, U, I>,
    #[pin]
    framed: Framed<T, U>,
    rx: mpsc::Receiver<Result<InnerMessage<I>, S::Error>>,
    tx: mpsc::Sender<Result<InnerMessage<I>, S::Error>>,
}

enum State<S: Service, U: Encoder<I> + Decoder, I> {
    Processing,
    Error(FramedDispatcherError<S::Error, U, I>),
    FramedError(FramedDispatcherError<S::Error, U, I>),
    FlushAndStop,
    Stopping,
}

impl<S: Service, U: Encoder<I> + Decoder, I> State<S, U, I> {
    fn take_error(&mut self) -> FramedDispatcherError<S::Error, U, I> {
        match mem::replace(self, State::Processing) {
            State::Error(err) => err,
            _ => panic!(),
        }
    }

    fn take_framed_error(&mut self) -> FramedDispatcherError<S::Error, U, I> {
        match mem::replace(self, State::Processing) {
            State::FramedError(err) => err,
            _ => panic!(),
        }
    }
}

impl<S, T, U, I> FramedDispatcher<S, T, U, I>
where
    S: Service<Request = <U as Decoder>::Item, Response = I>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder<I>,
    I: 'static,
    <U as Decoder>::Error: fmt::Debug,
    <U as Encoder<I>>::Error: fmt::Debug,
{
    pub fn new<F: IntoService<S>>(framed: Framed<T, U>, service: F) -> Self {
        let (tx, rx) = mpsc::channel();
        FramedDispatcher {
            framed,
            rx,
            tx,
            service: service.into_service(),
            state: State::Processing,
        }
    }

    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> bool
    where
        S: Service<Request = <U as Decoder>::Item, Response = I>,
        S::Error: 'static,
        S::Future: 'static,
        T: AsyncRead + AsyncWrite,
        U: Decoder + Encoder<I>,
        I: 'static,
        <U as Encoder<I>>::Error: fmt::Debug,
    {
        loop {
            let this = self.as_mut().project();
            match this.service.poll_ready(cx) {
                Poll::Ready(Ok(_)) => {
                    let item = match this.framed.next_item(cx) {
                        Poll::Ready(Some(Ok(el))) => el,
                        Poll::Ready(Some(Err(err))) => {
                            *this.state =
                                State::FramedError(FramedDispatcherError::Decoder(err));
                            return true;
                        }
                        Poll::Pending => return false,
                        Poll::Ready(None) => {
                            *this.state = State::Stopping;
                            return true;
                        }
                    };

                    let tx = this.tx.clone();
                    let fut = this.service.call(item);
                    actix_rt::spawn(async move {
                        let item = fut.await;
                        let _ = tx.send(item.map(InnerMessage::Item));
                    });
                }
                Poll::Pending => return false,
                Poll::Ready(Err(err)) => {
                    *this.state = State::Error(FramedDispatcherError::Service(err));
                    return true;
                }
            }
        }
    }

    /// write to framed object
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> bool
    where
        S: Service<Request = <U as Decoder>::Item, Response = I>,
        S::Error: 'static,
        S::Future: 'static,
        T: AsyncRead + AsyncWrite,
        U: Decoder + Encoder<I>,
        I: 'static,
        <U as Encoder<I>>::Error: fmt::Debug,
    {
        loop {
            let mut this = self.as_mut().project();
            while !this.framed.is_write_buf_full() {
                match Pin::new(&mut this.rx).poll_next(cx) {
                    Poll::Ready(Some(Ok(InnerMessage::Item(msg)))) => {
                        if let Err(err) = this.framed.as_mut().write(msg) {
                            *this.state =
                                State::FramedError(FramedDispatcherError::Encoder(err));
                            return true;
                        }
                    }
                    Poll::Ready(Some(Ok(InnerMessage::Close))) => {
                        *this.state = State::FlushAndStop;
                        return true;
                    }
                    Poll::Ready(Some(Err(err))) => {
                        *this.state = State::Error(FramedDispatcherError::Service(err));
                        return true;
                    }
                    Poll::Ready(None) | Poll::Pending => break,
                }
            }

            if !this.framed.is_write_buf_empty() {
                match this.framed.flush(cx) {
                    Poll::Pending => break,
                    Poll::Ready(Ok(_)) => (),
                    Poll::Ready(Err(err)) => {
                        debug!("Error sending data: {:?}", err);
                        *this.state = State::FramedError(FramedDispatcherError::Encoder(err));
                        return true;
                    }
                }
            } else {
                break;
            }
        }

        false
    }
}

impl<S, T, U, I> Future for FramedDispatcher<S, T, U, I>
where
    S: Service<Request = <U as Decoder>::Item, Response = I>,
    S::Error: 'static,
    S::Future: 'static,
    T: AsyncRead + AsyncWrite,
    U: Decoder + Encoder<I>,
    I: 'static,
    <U as Encoder<I>>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    type Output = Result<(), FramedDispatcherError<S::Error, U, I>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let this = self.as_mut().project();

            return match this.state {
                State::Processing => {
                    if self.as_mut().poll_read(cx) || self.as_mut().poll_write(cx) {
                        continue;
                    } else {
                        Poll::Pending
                    }
                }
                State::Error(_) => {
                    // flush write buffer
                    if !this.framed.is_write_buf_empty()
                        && this.framed.flush(cx).is_pending()
                    {
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(this.state.take_error()))
                }
                State::FlushAndStop => {
                    if !this.framed.is_write_buf_empty() {
                        match this.framed.flush(cx) {
                            Poll::Ready(Err(err)) => {
                                debug!("Error sending data: {:?}", err);
                                Poll::Ready(Ok(()))
                            }
                            Poll::Pending => Poll::Pending,
                            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
                        }
                    } else {
                        Poll::Ready(Ok(()))
                    }
                }
                State::FramedError(_) => {
                    Poll::Ready(Err(this.state.take_framed_error()))
                }
                State::Stopping => Poll::Ready(Ok(())),
            };
        }
    }
}
