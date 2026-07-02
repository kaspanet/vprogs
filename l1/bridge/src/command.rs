/// A boxed command applied to the sink on the worker thread, the bridge's only `&mut` writer.
pub type Command<T> = Box<dyn FnOnce(&mut T) + Send>;
