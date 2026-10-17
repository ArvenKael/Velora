use std::sync::Arc;

use serde_json::Value;

use crate::device::{Device, DeviceError, Outcome};
use crate::procedure::Procedure;

/// Every device the system can reach.
///
/// Procedures are named `device.procedure`, so a name says where it happens and
/// two devices cannot collide. This is also the list the model is shown: what it
/// can do is exactly what is here.
#[derive(Default, Clone)]
pub struct Catalog {
    devices: Vec<Arc<dyn Device>>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("devices", &self.names())
            .field("procedures", &self.procedures().len())
            .finish()
    }
}

impl Catalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Catalog::default()
    }

    /// Adds a device.
    pub fn with(mut self, device: impl Device + 'static) -> Self {
        self.add(Arc::new(device));
        self
    }

    /// Adds a device that is already shared.
    pub fn add(&mut self, device: Arc<dyn Device>) {
        self.devices.push(device);
    }

    /// The devices in reach.
    pub fn names(&self) -> Vec<&str> {
        self.devices.iter().map(|device| device.name()).collect()
    }

    /// Everything that can be asked for, named as the model will name it.
    pub fn procedures(&self) -> Vec<Procedure> {
        self.devices
            .iter()
            .flat_map(|device| {
                device
                    .procedures()
                    .iter()
                    .map(|procedure| procedure.qualified_as(device.name()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The procedure behind a qualified name.
    pub fn procedure(&self, qualified: &str) -> Option<Procedure> {
        let (device, procedure) = self.split(qualified)?;
        self.device_named(device)?
            .procedures()
            .iter()
            .find(|candidate| candidate.name() == procedure)
            .map(|candidate| candidate.qualified_as(device))
    }

    /// Whether anything here answers to that name.
    pub fn contains(&self, qualified: &str) -> bool {
        self.procedure(qualified).is_some()
    }

    /// Asks the device behind a name to do something.
    ///
    /// # Errors
    ///
    /// Fails if no device claims the name, or if the device could not attempt
    /// it.
    pub async fn invoke(
        &self,
        qualified: &str,
        arguments: &Value,
    ) -> Result<Outcome, DeviceError> {
        let (device_name, procedure) = self
            .split(qualified)
            .ok_or_else(|| DeviceError::UnknownProcedure(qualified.to_owned()))?;

        let device = self
            .device_named(device_name)
            .ok_or_else(|| DeviceError::UnknownProcedure(qualified.to_owned()))?;

        device.invoke(procedure, arguments).await
    }

    fn device_named(&self, name: &str) -> Option<&Arc<dyn Device>> {
        self.devices.iter().find(|device| device.name() == name)
    }

    /// Splits `device.procedure` at the first dot.
    fn split<'a>(&self, qualified: &'a str) -> Option<(&'a str, &'a str)> {
        let (device, procedure) = qualified.split_once('.')?;
        if device.is_empty() || procedure.is_empty() {
            return None;
        }
        Some((device, procedure))
    }
}
