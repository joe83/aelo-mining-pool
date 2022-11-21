mod client;
pub use client::*;


#[macro_use]
extern crate async_trait;
pub use async_trait::async_trait;

// 实现观察者模式。
struct block {}

pub trait Observer {
    fn update();
}

pub trait Subject {
    fn NotifyObserver();
}
