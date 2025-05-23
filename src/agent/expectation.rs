use std::{fmt::Display, fs::File, path::Path};

use anyhow::{anyhow, Context};
use burn::{
    config::Config,
    data::dataloader::batcher::Batcher,
    lr_scheduler::LrScheduler,
    module::{AutodiffModule, ParamId},
    nn::loss::{HuberLossConfig, MseLoss},
    optim::{
        adaptor::OptimizerAdaptor,
        record::{AdaptorRecord, AdaptorRecordItem},
        GradientsParams, Optimizer, SimpleOptimizer,
    },
    record::{CompactRecorder, HalfPrecisionSettings, Record, Recorder},
    tensor::{backend::AutodiffBackend, ElementConversion, Shape, Tensor, TensorData},
};

use crate::{
    batch::DeepQNetworkBathcer, Action, ActionSpace, Agent, DeepQNetworkState, Estimator,
    Experience, ObservationSpace, PrioritizedReplay, PrioritizedReplayAgent,
};

use super::LossFunction;

#[derive(Debug, Config)]
pub struct DeepQNetworkAgentConfig {
    teacher_update_freq: usize,
    n_step: usize,
    double_dqn: bool,
    loss_function: LossFunction,
}

#[derive(Clone)]
pub struct DeepQNetworkAgent<
    B: AutodiffBackend,
    const D: usize,
    M: AutodiffModule<B>,
    O: SimpleOptimizer<B::InnerBackend>,
    S: LrScheduler,
> {
    model: M,
    teacher_model: M,
    optimizer: OptimizerAdaptor<O, M, B>,
    lr_scheduler: S,
    observation_space: ObservationSpace<D>,
    action_space: ActionSpace,
    device: B::Device,
    update_counter: usize,

    config: DeepQNetworkAgentConfig,
}

impl<
        B: AutodiffBackend,
        const D: usize,
        M: AutodiffModule<B> + Estimator<B>,
        O: SimpleOptimizer<B::InnerBackend>,
        S: LrScheduler,
    > DeepQNetworkAgent<B, D, M, O, S>
{
    pub fn new(
        model: M,
        optimizer: OptimizerAdaptor<O, M, B>,
        lr_scheduler: S,
        observation_space: ObservationSpace<D>,
        action_space: ActionSpace,
        device: B::Device,
        config: DeepQNetworkAgentConfig,
    ) -> Self {
        let teacher_model = model.clone().fork(&device);
        Self {
            model,
            teacher_model,
            optimizer,
            lr_scheduler,
            observation_space,
            action_space,
            device,
            update_counter: 0,
            config,
        }
    }
}

impl<B, const D: usize, M, O, S> PrioritizedReplay<DeepQNetworkState>
    for DeepQNetworkAgent<B, D, M, O, S>
where
    B: AutodiffBackend,
    M: AutodiffModule<B> + Display + Estimator<B>,
    M::InnerModule: Estimator<B::InnerBackend>,
    O: SimpleOptimizer<B::InnerBackend>,
    S: LrScheduler + Clone,
{
    fn temporaral_difference_error(
        &self,
        gamma: f32,
        experiences: &[Experience<DeepQNetworkState>],
    ) -> anyhow::Result<Vec<f32>> {
        let batcher = DeepQNetworkBathcer::new(self.device.clone(), self.action_space);

        let mut shape = *self.observation_space.shape();
        shape[0] = experiences.len();

        let model = self.model.clone();
        let item = batcher.batch(experiences.to_vec());
        let observation = item.observation.clone();
        let q_value = model.predict(observation.reshape(shape));
        let next_target_q_value = self
            .teacher_model
            .valid()
            .predict(item.next_observation.clone().inner().reshape(shape));
        let next_target_q_value = match self.action_space {
            ActionSpace::Discrete(num_class) => {
                if self.config.double_dqn {
                    let next_q_value = model
                        .valid()
                        .predict(item.next_observation.clone().inner().reshape(shape));
                    let next_actions = next_q_value.argmax(1);
                    next_target_q_value
                        .gather(1, next_actions)
                        .repeat_dim(1, num_class as usize)
                } else {
                    next_target_q_value
                        .max_dim(1)
                        .repeat_dim(1, num_class as usize)
                }
            }
        };
        let next_target_q_value: Tensor<B, 2> =
            Tensor::from_inner(next_target_q_value).to_device(&self.device);
        let targets = q_value.clone().inner()
            * (item.action.ones_like().inner() - item.action.clone().inner())
            + (next_target_q_value
                .clone()
                .inner()
                .mul_scalar(gamma.powi(self.config.n_step as i32))
                * (item.done.ones_like().inner() - item.done.clone().inner())
                + item.reward.clone().inner())
                * item.action.clone().inner();
        let td: Vec<f32> = (q_value.inner() - targets)
            .abs()
            .sum_dim(1)
            .into_data()
            .to_vec()
            .map_err(|e| anyhow!("tensor data to_vec error: {:?}", e))?;
        Ok(td)
    }
}

impl<B, const D: usize, M, O, S> Agent<DeepQNetworkState> for DeepQNetworkAgent<B, D, M, O, S>
where
    B: AutodiffBackend,
    M: AutodiffModule<B> + Display + Estimator<B>,
    M::InnerModule: Estimator<B::InnerBackend>,
    O: SimpleOptimizer<B::InnerBackend>,
    S: LrScheduler + Clone,
{
    fn policy(&self, observation: &[f32]) -> Action {
        let shape = *self.observation_space.shape();
        let feature: Tensor<<B as AutodiffBackend>::InnerBackend, D> = Tensor::from_data(
            TensorData::new(observation.to_vec(), Shape::new(shape)).convert::<B::FloatElem>(),
            &self.device,
        );
        let scores = self.model.valid().predict(feature);
        println!("score: {:?}", scores.to_data().to_vec::<f32>());
        match self.action_space {
            ActionSpace::Discrete(..) => {
                let scores = scores.argmax(1);
                let scores = scores.flatten::<1>(0, 1).into_scalar();
                Action::Discrete(scores.elem())
            }
        }
    }

    fn update(
        &mut self,
        gamma: f32,
        experiences: &[Experience<DeepQNetworkState>],
        weights: &[f32],
    ) -> anyhow::Result<()> {
        let batcher = DeepQNetworkBathcer::new(self.device.clone(), self.action_space);

        let batch_size = experiences.len();
        let mut shape = *self.observation_space.shape();
        shape[0] = batch_size;

        let model = self.model.clone();
        let item = batcher.batch(experiences.to_vec());
        let observation = item.observation.clone();
        let q_value = model.predict(observation.reshape(shape));
        let next_target_q_value = self
            .teacher_model
            .valid()
            .predict(item.next_observation.clone().inner().reshape(shape));
        let next_target_q_value: Tensor<B, 2> =
            Tensor::from_inner(next_target_q_value).to_device(&self.device);
        let next_target_q_value = match self.action_space {
            ActionSpace::Discrete(num_class) => {
                if self.config.double_dqn {
                    let next_q_value = model.predict(item.next_observation.clone().reshape(shape));
                    let next_actions = next_q_value.argmax(1);
                    next_target_q_value
                        .gather(1, next_actions)
                        .repeat_dim(1, num_class as usize)
                } else {
                    next_target_q_value
                        .max_dim(1)
                        .repeat_dim(1, num_class as usize)
                }
            }
        };
        let targets = (next_target_q_value.clone().inner()
            * (item.done.ones_like().inner() - item.done.clone().inner()))
        .mul_scalar(gamma.powi(self.config.n_step as i32))
            + item.reward.clone().inner();
        let targets = q_value.clone().inner()
            * (item.action.ones_like().inner() - item.action.clone().inner())
            + targets * item.action.clone().inner();
        let targets = Tensor::from_inner(targets);
        let loss = match self.config.loss_function {
            LossFunction::Huber => HuberLossConfig::new(1.0)
                .init()
                .forward_no_reduction(q_value, targets),
            LossFunction::Squared => MseLoss::new().forward_no_reduction(q_value, targets),
        };
        let weights = Tensor::from_data(
            TensorData::new(weights.to_vec(), Shape::new([weights.len(), 1]))
                .convert::<B::FloatElem>(),
            &self.device,
        );
        let loss = loss.sum_dim(1) * weights;
        let loss = loss.mean();
        let grads: <B as AutodiffBackend>::Gradients = loss.backward();
        let grads = GradientsParams::from_grads(grads, &model);
        self.model = self.optimizer.step(self.lr_scheduler.step(), model, grads);

        self.update_counter += 1;
        if self.update_counter % self.config.teacher_update_freq == 0 {
            self.teacher_model = self.model.clone().fork(&self.device);
        }

        Ok(())
    }

    fn make_state(&self, next_observation: &[f32], state: &DeepQNetworkState) -> DeepQNetworkState {
        DeepQNetworkState {
            observation: state.next_observation.clone(),
            next_observation: next_observation.to_vec(),
        }
    }

    fn save<P: AsRef<Path>>(&self, artifacts_dir: P) -> anyhow::Result<()> {
        let artifacts_dir = artifacts_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&artifacts_dir)
            .with_context(|| format!("fail to create {:?}", artifacts_dir))?;
        self.model
            .clone()
            .save_file(artifacts_dir.join("model"), &CompactRecorder::new())
            .with_context(|| "fail to save model")?;
        let optimizer_record = self.optimizer.to_record();
        let optimizer_record = optimizer_record
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.into_item()))
            .collect::<hashbrown::HashMap<String, AdaptorRecordItem<O, B, HalfPrecisionSettings>>>(
            );

        let mut optimizer_file = File::create(artifacts_dir.join("optimizer.mpk"))
            .with_context(|| "create optimizer file")?;

        rmp_serde::encode::write(&mut optimizer_file, &optimizer_record)
            .with_context(|| "Failed to write optimizer record")?;

        let scheduler_record = self.lr_scheduler.to_record();
        let scheduler_record: <<S as LrScheduler>::Record<B> as Record<_>>::Item<
            HalfPrecisionSettings,
        > = scheduler_record.into_item();
        let mut scheduler_file = File::create(artifacts_dir.join("scheduler.mpk"))
            .with_context(|| "create scheduler file")?;
        rmp_serde::encode::write(&mut scheduler_file, &scheduler_record)
            .with_context(|| "Failed to write scheduler record")?;
        Ok(())
    }

    fn load<P: AsRef<Path>>(&mut self, restore_dir: P) -> anyhow::Result<()> {
        let restore_dir = restore_dir.as_ref().to_path_buf();
        let model_file = restore_dir.join("model.mpk");
        if model_file.exists() {
            let record = CompactRecorder::new()
                .load(model_file, &self.device)
                .with_context(|| "Failed to load model")?;
            self.model = self.model.clone().load_record(record);
        }
        let optimizer_file = restore_dir.join("optimizer.mpk");
        if optimizer_file.exists() {
            let optimizer_file =
                File::open(optimizer_file).with_context(|| "open optimizer file")?;
            let record: hashbrown::HashMap<String, AdaptorRecordItem<O, B, HalfPrecisionSettings>> =
                rmp_serde::decode::from_read(optimizer_file)
                    .with_context(|| "Failed to read optimizer record")?;
            let record = record
                .into_iter()
                .map(|(k, v)| {
                    (
                        ParamId::deserialize(k.as_str()),
                        AdaptorRecord::from_item(v, &self.device),
                    )
                })
                .collect::<hashbrown::HashMap<_, _>>();
            self.optimizer = self.optimizer.clone().load_record(record);
        }
        let scheduler_file = restore_dir.join("scheduler.mpk");
        if scheduler_file.exists() {
            let scheduler_file =
                File::open(scheduler_file).with_context(|| "open scheduler file")?;
            let record: <<S as LrScheduler>::Record<B> as Record<_>>::Item<HalfPrecisionSettings> =
                rmp_serde::decode::from_read(scheduler_file)
                    .with_context(|| "Failed to read scheduler record")?;
            let record =
                <<S as LrScheduler>::Record<B> as Record<_>>::from_item(record, &self.device);
            self.lr_scheduler = self.lr_scheduler.clone().load_record(record);
        }

        Ok(())
    }
}

impl<B, const D: usize, M, O, S> PrioritizedReplayAgent<DeepQNetworkState>
    for DeepQNetworkAgent<B, D, M, O, S>
where
    B: AutodiffBackend,
    M: AutodiffModule<B> + Display + Estimator<B>,
    M::InnerModule: Estimator<B::InnerBackend>,
    O: SimpleOptimizer<B::InnerBackend>,
    S: LrScheduler + Clone,
{
}
