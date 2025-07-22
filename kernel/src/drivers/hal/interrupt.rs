use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

pub type InterruptVector = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptError {
    VectorNotFound,
    HandlerNotSet,
    InvalidVector,
    ControllerError,
}

impl fmt::Display for InterruptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InterruptError::VectorNotFound => write!(f, "Interrupt vector not found"),
            InterruptError::HandlerNotSet => write!(f, "Interrupt handler not set"),
            InterruptError::InvalidVector => write!(f, "Invalid interrupt vector"),
            InterruptError::ControllerError => write!(f, "Interrupt controller error"),
        }
    }
}

pub trait InterruptHandler: Send + Sync {
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
    fn can_handle(&self, vector: InterruptVector) -> bool;
    fn priority(&self) -> u8 {
        0
    }
}

pub trait InterruptController: Send + Sync {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError>;
    
    fn unregister_handler(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;
    
    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;
    fn disable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;
    
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError>;
    
    fn pending_interrupts(&self) -> Vec<InterruptVector>;
    fn acknowledge_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError>;
}

pub struct SimpleInterruptHandler<F>
where
    F: Fn(InterruptVector) -> Result<(), InterruptError> + Send + Sync,
{
    handler_fn: F,
    vector: InterruptVector,
}

impl<F> SimpleInterruptHandler<F>
where
    F: Fn(InterruptVector) -> Result<(), InterruptError> + Send + Sync,
{
    pub fn new(vector: InterruptVector, handler_fn: F) -> Self {
        Self {
            handler_fn,
            vector,
        }
    }
}

impl<F> InterruptHandler for SimpleInterruptHandler<F>
where
    F: Fn(InterruptVector) -> Result<(), InterruptError> + Send + Sync,
{
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        (self.handler_fn)(vector)
    }
    
    fn can_handle(&self, vector: InterruptVector) -> bool {
        vector == self.vector
    }
}

use spin::Mutex;
use alloc::collections::BTreeMap;

pub struct BasicInterruptController {
    handlers: Mutex<BTreeMap<InterruptVector, Arc<dyn InterruptHandler>>>,
    enabled_interrupts: Mutex<alloc::collections::BTreeSet<InterruptVector>>,
}

impl BasicInterruptController {
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(BTreeMap::new()),
            enabled_interrupts: Mutex::new(alloc::collections::BTreeSet::new()),
        }
    }
}

impl InterruptController for BasicInterruptController {
    fn register_handler(
        &mut self,
        vector: InterruptVector,
        handler: Arc<dyn InterruptHandler>,
    ) -> Result<(), InterruptError> {
        let mut handlers = self.handlers.lock();
        handlers.insert(vector, handler);
        Ok(())
    }
    
    fn unregister_handler(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        let mut handlers = self.handlers.lock();
        handlers.remove(&vector);
        
        let mut enabled = self.enabled_interrupts.lock();
        enabled.remove(&vector);
        
        Ok(())
    }
    
    fn enable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        let handlers = self.handlers.lock();
        if !handlers.contains_key(&vector) {
            return Err(InterruptError::HandlerNotSet);
        }
        
        let mut enabled = self.enabled_interrupts.lock();
        enabled.insert(vector);
        
        Ok(())
    }
    
    fn disable_interrupt(&mut self, vector: InterruptVector) -> Result<(), InterruptError> {
        let mut enabled = self.enabled_interrupts.lock();
        enabled.remove(&vector);
        Ok(())
    }
    
    fn handle_interrupt(&self, vector: InterruptVector) -> Result<(), InterruptError> {
        let enabled = self.enabled_interrupts.lock();
        if !enabled.contains(&vector) {
            return Ok(());
        }
        
        let handlers = self.handlers.lock();
        if let Some(handler) = handlers.get(&vector) {
            handler.handle_interrupt(vector)
        } else {
            Err(InterruptError::HandlerNotSet)
        }
    }
    
    fn pending_interrupts(&self) -> Vec<InterruptVector> {
        Vec::new()
    }
    
    fn acknowledge_interrupt(&mut self, _vector: InterruptVector) -> Result<(), InterruptError> {
        Ok(())
    }
}