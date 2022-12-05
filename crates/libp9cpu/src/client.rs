

pub type P9cpuCommand = crate::rpc::P9cpuStartRequest;

pub trait P9cpuClient {
    fn dial(&mut self);
    fn run(&mut self, command: P9cpuCommand);
    fn wait(&mut self);
}
