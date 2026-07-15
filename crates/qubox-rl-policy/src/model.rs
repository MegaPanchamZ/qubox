use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{linear, Linear, VarBuilder};

use crate::observation::Observation;

pub const N_ACTIONS: usize = 96;

pub struct PolicyModel {
    fc1: Linear,
    fc2: Linear,
    fc3: Linear,
    fc_out: Linear,
    device: Device,
}

impl PolicyModel {
    pub fn load(ckpt_path: &std::path::Path) -> std::io::Result<Self> {
        let device = Device::Cpu;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[ckpt_path.to_str().unwrap()], DType::F32, &device)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
        };
        let fc1 = linear(11, 128, vb.pp("fc1"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let fc2 = linear(128, 128, vb.pp("fc2"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let fc3 = linear(128, 128, vb.pp("fc3"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let fc_out = linear(128, N_ACTIONS, vb.pp("fc_out"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok(Self {
            fc1,
            fc2,
            fc3,
            fc_out,
            device,
        })
    }

    pub fn forward(&self, obs: &Observation) -> candle_core::Result<Tensor> {
        let v = obs.to_normalised_vec();
        let x = Tensor::from_slice(&v, (1, 11), &self.device)?;
        let x = self.fc1.forward(&x)?.relu()?;
        let x = self.fc2.forward(&x)?.relu()?;
        let x = self.fc3.forward(&x)?.relu()?;
        let logits = self.fc_out.forward(&x)?;
        Ok(logits)
    }

    pub fn infer_argmax(&self, obs: &Observation) -> usize {
        let logits = self.forward(obs).unwrap();
        logits.argmax(1).unwrap().to_vec1::<u32>().unwrap()[0] as usize
    }
}
