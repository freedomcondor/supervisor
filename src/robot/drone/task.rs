use std::{collections::HashMap, time::Duration};
use anyhow::Context;
use ansi_parser::{Output, AnsiParser}; //AnsiSequence
use bytes::BytesMut;
use mavlink::{MavHeader, common::{self, MavMessage, SerialControlDev, SerialControlFlag}};
use tokio::{net::TcpStream, sync::{broadcast, mpsc, oneshot}};
use futures::{FutureExt, SinkExt, Stream, StreamExt, TryStreamExt};
use tokio_stream::{self, wrappers::ReceiverStream};
use tokio_util::codec::Framed;

use crate::network::{fernbedienung, fernbedienung_ext::MjpegStreamerStream, xbee};
use crate::robot::{FernbedienungAction, XbeeAction, TerminalAction};
use super::codec;

pub use shared::{
    drone::{Descriptor, Update},
    experiment::software::Software
};

const DRONE_BATT_FULL_MV: f32 = 4050.0;
const DRONE_BATT_EMPTY_MV: f32 = 3500.0;
const DRONE_BATT_NUM_CELLS: f32 = 3.0;
const DRONE_CAMERAS_CONFIG: &[(&str, u16, u16, u16)] = &[
    ("/dev/camera0", 1024, 768, 8000),
    ("/dev/camera1", 1024, 768, 8001),
    ("/dev/camera2", 1024, 768, 8002),
    ("/dev/camera3", 1024, 768, 8003),
];

const XBEE_DEFAULT_PIN_CONFIG: &[(xbee::Pin, xbee::PinMode)] = &[
    /* UART pins: TX: DOUT, RTS: DIO6, RX: DIN, CTS: DIO7 */
    /* UART enabled without hardware flow control */
    (xbee::Pin::DOUT, xbee::PinMode::Alternate),
    (xbee::Pin::DIO6, xbee::PinMode::Disable),
    (xbee::Pin::DIO7, xbee::PinMode::Disable),
    (xbee::Pin::DIN,  xbee::PinMode::Alternate),
    /* Input pins for reading an identifer */
    (xbee::Pin::DIO0, xbee::PinMode::Input),
    (xbee::Pin::DIO1, xbee::PinMode::Input),
    (xbee::Pin::DIO2, xbee::PinMode::Input),
    (xbee::Pin::DIO3, xbee::PinMode::Input),
    /* Output pins for controlling power and mux */
    (xbee::Pin::DIO4, xbee::PinMode::OutputDefaultLow),
    (xbee::Pin::DIO11, xbee::PinMode::OutputDefaultLow),
    (xbee::Pin::DIO12, xbee::PinMode::OutputDefaultLow),
];

/* hardware flow control connected but disabled */

#[derive(Debug)]
pub enum Action {
    AssociateFernbedienung(fernbedienung::Device),
    AssociateXbee(xbee::Device),
    ExecuteXbeeAction(oneshot::Sender<anyhow::Result<()>>, XbeeAction),
    ExecuteFernbedienungAction(oneshot::Sender<anyhow::Result<()>>, FernbedienungAction),
    Subscribe(oneshot::Sender<broadcast::Receiver<Update>>),
    // its good to keep this one seperate since start exp need to interact with xbee and fernbedienung
    UploadExperiment(oneshot::Sender<anyhow::Result<()>>, Software),
    StartExperiment(oneshot::Sender<anyhow::Result<()>>),
    StopExperiment,
}

pub type Sender = mpsc::Sender<Action>;
pub type Receiver = mpsc::Receiver<Action>;

fn xbee_pin_states_stream<'dev>(
    device: &'dev xbee::Device
) -> impl Stream<Item = anyhow::Result<HashMap<xbee::Pin, bool>>> + 'dev {
    async_stream::stream! {
        let mut attempts: u8 = 0;
        loop {
            let link_margin_task = tokio::time::timeout(Duration::from_millis(500), device.pin_states()).await
                .context("Timeout while communicating with Xbee")
                .and_then(|result| result.context("Could not communicate with Xbee"));
            match link_margin_task {
                Ok(response) => {
                    attempts = 0;
                    yield Ok(response);
                },
                Err(error) => match attempts {
                    0..=2 => attempts += 1,
                    _ => yield Err(error)
                }
            }
        }
    }
}

fn xbee_link_margin_stream<'dev>(
    device: &'dev xbee::Device
) -> impl Stream<Item = anyhow::Result<i32>> + 'dev {
    async_stream::stream! {
        let mut attempts: u8 = 0;
        loop {
            let link_margin_task = tokio::time::timeout(Duration::from_millis(500), device.link_margin()).await
                .context("Timeout while communicating with Xbee")
                .and_then(|result| result.context("Xbee communication error"));
            match link_margin_task {
                Ok(response) => {
                    attempts = 0;
                    yield Ok(response);
                },
                Err(error) => match attempts {
                    0..=2 => attempts += 1,
                    _ => yield Err(error)
                }
            }
        }
    }
}

async fn mavlink(
    device: &xbee::Device,
    mut rx: mpsc::Receiver<(oneshot::Sender<anyhow::Result<()>>, TerminalAction)>,
    updates_tx: broadcast::Sender<Update>
) -> anyhow::Result<()> {
    let connection = TcpStream::connect((device.addr, 9750))
        .map(|result| result
            .context("Could not connect to serial communication service"));
    let connection = tokio::time::timeout(Duration::from_secs(1), connection)
        .map(|result| result
            .context("Timeout while connecting to serial communication service")
            .and_then(|result| result)).await?;
    let (sink, mut stream) = 
        Framed::new(connection, codec::MavMessageCodec::<MavMessage>::new()).split();
    let mut mavlink_sequence = 0u8;
    let sink = sink.with(|message| async {
        let header = MavHeader { system_id: 255, component_id: 190, sequence: 0 };
            anyhow::Result::<_>::Ok((header, message))
    });
    tokio::pin!(sink);
    /* heartbeat stream */
    let heartbeat_stream = futures::stream::iter(std::iter::repeat(
        MavMessage::HEARTBEAT(common::HEARTBEAT_DATA {
            custom_mode: 0,
            mavtype: common::MavType::MAV_TYPE_GCS,
            autopilot: common::MavAutopilot::MAV_AUTOPILOT_GENERIC,
            base_mode: common::MavModeFlag::empty(),
            system_status: common::MavState::MAV_STATE_UNINIT,
            mavlink_version: 3,
        })
    ));
    let heartbeat_stream_throttled =
        tokio_stream::StreamExt::throttle(heartbeat_stream, Duration::from_millis(500));
    tokio::pin!(heartbeat_stream_throttled);

    loop {
        tokio::select! {
            Some(heartbeat) = heartbeat_stream_throttled.next() => {
                let _ = sink.send(heartbeat).await;
            }
            Some((callback, action)) = rx.recv() => {
                match action {
                    // note that commands should not be run when mavlink is being used by ARGoS
                    TerminalAction::Run(command) => {
                        let command_padded = command.as_bytes()
                            .iter()
                            .cloned()
                            .chain(std::iter::repeat(0u8))
                            .take(70)
                            .collect::<Vec<_>>();

                        log::info!("sending \"{}\"", command);
                        let data = common::SERIAL_CONTROL_DATA {
                            baudrate: 0,
                            timeout: 0,
                            device: SerialControlDev::SERIAL_CONTROL_DEV_SHELL,
                            flags: SerialControlFlag::SERIAL_CONTROL_FLAG_EXCLUSIVE | 
                                SerialControlFlag::SERIAL_CONTROL_FLAG_RESPOND |
                                SerialControlFlag::SERIAL_CONTROL_FLAG_MULTI,
                            count: command_padded.len() as u8,
                            data: command_padded,
                        };
                        log::info!("sending {} bytes", data.data.len());
                        let message = MavMessage::SERIAL_CONTROL(data);
                        match sink.send(message).await {
                            Ok(_) => {
                                mavlink_sequence = mavlink_sequence.wrapping_add(1);
                            }
                            Err(error) => log::error!("{}", error)
                        }
                    },
                    _ => {}, // ignore start and stop
                }
            },
            Some(Ok((header, body))) = stream.next() => {
                match body {
                    MavMessage::HEARTBEAT(_) => {
                        log::info!("heartbeat from {}:{}", header.system_id, header.component_id);
                    },
                    MavMessage::BATTERY_STATUS(data) => {
                        let mut battery_reading = data.voltages[0] as f32;
                        battery_reading /= DRONE_BATT_NUM_CELLS;
                        battery_reading -= DRONE_BATT_EMPTY_MV;
                        battery_reading /= DRONE_BATT_FULL_MV - DRONE_BATT_EMPTY_MV;
                        let battery_reading = (battery_reading.max(0.0).min(1.0) * 100.0) as i32;
                        let _ = updates_tx.send(Update::Battery(battery_reading));
                    },
                    MavMessage::SERIAL_CONTROL(common::SERIAL_CONTROL_DATA { data, count, .. }) => {
                        log::info!("got serial control data");
                        let valid = match std::str::from_utf8(&data[..count as usize]) {
                            Ok(data) => data,
                            Err(error) => {
                                log::info!("count included non-utf8 data");
                                std::str::from_utf8(&data[..error.valid_up_to()]).unwrap()
                            }
                        };
                        log::info!("received \"{}\"", valid);
                        let parsed: Vec<Output> = valid
                            .ansi_parse()
                            .collect();
                        log::info!("received \"{:?}\"", parsed);
                        // https://github.com/mavlink/qgroundcontrol/blob/master/src/AnalyzeView/MavlinkConsoleController.cc#L168
                        //let s = .into_owned();
                        //let _  = updates_tx.send(Update::Mavlink(s));
                    },
                    _ => {}
                }
            }
        }
    }
}



async fn xbee(
    device: xbee::Device,
    mut rx: mpsc::Receiver<(oneshot::Sender<anyhow::Result<()>>, XbeeAction)>,
    updates_tx: broadcast::Sender<Update>
) -> anyhow::Result<()> {
    device.set_pin_modes(XBEE_DEFAULT_PIN_CONFIG).await
        .context("Could not set Xbee pin modes")?;
    /* set the serial communication service to TCP mode */
    device.set_scs_mode(true).await
        .context("Could not enable serial communication service")?;
    /* set the baud rate to match the baud rate of the Pixhawk */
    device.set_baud_rate(115200).await
        .context("Could not set serial baud rate")?;
    /* link margin stream */
    let link_margin_stream = xbee_link_margin_stream(&device);
    let link_margin_stream_throttled =
        tokio_stream::StreamExt::throttle(link_margin_stream, Duration::from_millis(1000));       
    tokio::pin!(link_margin_stream_throttled);
    /* pin states stream */
    let pin_states_stream = xbee_pin_states_stream(&device);
    let pin_states_stream_throttled =
        tokio_stream::StreamExt::throttle(pin_states_stream, Duration::from_millis(1000));       
    tokio::pin!(pin_states_stream_throttled);
    /* mavlink task */
    let (mut mavlink_tx, mavlink_rx) = mpsc::channel(8);
    let mavlink_task = mavlink(&device, mavlink_rx, updates_tx.clone());
    tokio::pin!(mavlink_task);

    loop {
        tokio::select! {
            Some(response) = link_margin_stream_throttled.next() => {
                let update = Update::XbeeSignal(response?);
                let _ = updates_tx.send(update);
            },
            Some(response) = pin_states_stream_throttled.next() => {
                let response = response?;
                let upcore = response.get(&xbee::Pin::DIO11);
                let pixhawk = response.get(&xbee::Pin::DIO12);
                match (upcore, pixhawk) {
                    (Some(&upcore), Some(&pixhawk)) => {
                        let _ = updates_tx.send(Update::PowerState { upcore, pixhawk });
                    },
                    _ => log::warn!("Could not update power state")
                }
            },
            recv = rx.recv() => match recv {
                Some((callback, action)) => match action {
                    XbeeAction::SetUpCorePower(enable) => {
                        let result = device.write_outputs(&[(xbee::Pin::DIO11, enable)]).await
                            .context("Could not configure Up Core power");
                        let _ = callback.send(result);
                    },
                    XbeeAction::SetPixhawkPower(enable) => {
                        let result = device.write_outputs(&[(xbee::Pin::DIO12, enable)]).await
                            .context("Could not configure Pixhawk power");
                        let _ = callback.send(result);
                    },
                    XbeeAction::Mavlink(action) => {
                        if let Err(mpsc::error::SendError((callback, action))) = mavlink_tx.send((callback, action)).await {
                            let _ = callback.send(Err(anyhow::anyhow!("Could not send {:?} to MAVLink terminal", action)));
                        }
                    },
                },
                None => break Ok(()), // normal shutdown
            },
            result = &mut mavlink_task => {
                if let Err(error) = result {
                    log::error!("Mavlink task terminated: {}", error);
                }
                /* restart task */
                let (tx, rx) = mpsc::channel(8);
                mavlink_tx = tx;
                mavlink_task.set(mavlink(&device, rx, updates_tx.clone()));
            }
        }
    }
}

fn fernbedienung_link_strength_stream<'dev>(
    device: &'dev fernbedienung::Device
) -> impl Stream<Item = anyhow::Result<i32>> + 'dev {
    async_stream::stream! {
        let mut attempts : u8 = 0;
        loop {
            let link_strength_task = tokio::time::timeout(Duration::from_millis(500), device.link_strength()).await
                .context("Timeout while communicating with Up Core")
                .and_then(|result| result.context("Could not communicate with Up Core"));
            match link_strength_task {
                Ok(response) => {
                    attempts = 0;
                    yield Ok(response);
                },
                Err(error) => match attempts {
                    0..=2 => attempts += 1,
                    _ => yield Err(error)
                }
            }
        }
    }
}

async fn bash(
    device: &fernbedienung::Device,
    mut rx: mpsc::Receiver<(oneshot::Sender<anyhow::Result<()>>, TerminalAction)>,
    updates_tx: broadcast::Sender<Update>,
) {   
    let process = futures::future::pending().left_future();
    let stdout = futures::stream::pending().left_stream();
    let stderr = futures::stream::pending().left_stream();
    let mut stdin = None;
    let mut terminate = None;
    tokio::pin!(process);
    tokio::pin!(stdout);
    tokio::pin!(stderr);
    loop {
        tokio::select! {
            Some((callback, action)) = rx.recv() => match action {
                TerminalAction::Start => {
                    /* set up channels */
                    let (stdout_tx, stdout_rx) = mpsc::channel(8);
                    stdout.set(ReceiverStream::new(stdout_rx).right_stream());
                    let (stderr_tx, stderr_rx) = mpsc::channel(8);
                    stderr.set(ReceiverStream::new(stderr_rx).right_stream());
                    let (stdin_tx, stdin_rx) = mpsc::channel(8);
                    stdin = Some(stdin_tx);
                    let (terminate_tx, terminate_rx) = oneshot::channel();
                    terminate = Some(terminate_tx);
                    /* start process */
                    let bash = fernbedienung::Process {
                        target: "bash".into(),
                        working_dir: None,
                        args: vec!["-li".to_owned()],
                    };
                    process.set(device.run(bash, terminate_rx, stdin_rx, stdout_tx, stderr_tx).right_future());
                    log::info!("Remote Bash instance started");
                },
                TerminalAction::Run(mut command) => if let Some(tx) = stdin.as_ref() {
                    command.push_str("\r");
                    let _  = tx.send(BytesMut::from(command.as_bytes())).await;
                },
                TerminalAction::Stop => if let Some(tx) = terminate.take() {
                    let _ = tx.send(());
                }
            },
            result = &mut process => {
                process.set(futures::future::pending().left_future());
                stdout.set(futures::stream::pending().left_stream());
                stderr.set(futures::stream::pending().left_stream());
                stdin = None;
                terminate = None;
                log::info!("Remote Bash instance terminated with {:?}", result);
            }
            Some(stdout) = stdout.next() => {
                let update = Update::Bash(String::from_utf8_lossy(&stdout).into_owned());
                log::info!("{:?}", update);
                let _ = updates_tx.send(update);
            },
            Some(stderr) = stderr.next() => {
                let update = Update::Bash(String::from_utf8_lossy(&stderr).into_owned());
                log::info!("{:?}", update);
                let _ = updates_tx.send(update);
            },
        }
    }
}

// async fn experiment(device: &fernbedienung::Device,
//     mut rx: mpsc::Receiver<(oneshot::Sender<anyhow::Result<()>>, ExperimentAction)>,
//     updates_tx: broadcast::Sender<Update>) {


//     // upload software

//     // start argos

//     // forward stderr/stdout to journal

//     // terminate if either terminate_rx is sent or ARGoS quits

//     loop {
//         tokio::select! {
//             _ = terminate_rx => break,
//         }
//     }

// }

// can start ARGoS be included as a fbaction?
// the main issue is that I need to pass the journal to the action
// what if updates_tx included ARGoSUpdate which journal just subscribed to?

// terminate channel -> this is easy or even irrelevant now



// update software



async fn fernbedienung(
    device: fernbedienung::Device,
    mut rx: mpsc::Receiver<(oneshot::Sender<anyhow::Result<()>>, FernbedienungAction)>,
    updates_tx: broadcast::Sender<Update>
) {
    /* bash task */
    let (mut bash_tx, bash_rx) = mpsc::channel(8);
    let bash_task = bash(&device, bash_rx, updates_tx.clone());
    tokio::pin!(bash_task);
    /* experiment task */
    // let (mut experiment_tx, experiment_rx) = mpsc::channel(8);
    // let experiment_task = bash(&device, experiment_rx, updates_tx.clone());
    // tokio::pin!(experiment_task);
    /* link strength stream */
    let link_strength_stream = fernbedienung_link_strength_stream(&device)
        .map_ok(Update::FernbedienungSignal);
    let link_strength_stream_throttled =
        tokio_stream::StreamExt::throttle(link_strength_stream, Duration::from_millis(1000));
    tokio::pin!(link_strength_stream_throttled);
    /* camera stream */
    let mut cameras_stream: tokio_stream::StreamMap<String, _> =
        tokio_stream::StreamMap::new();
    loop {
        tokio::select! {
            Some((camera, result)) = cameras_stream.next() => {
                let result: reqwest::Result<bytes::Bytes> = result;
                let update = Update::Camera { camera, result: result.map_err(|e| e.to_string()) };
                let _ = updates_tx.send(update);
            },
            Some(response) = link_strength_stream_throttled.next() => match response {
                Ok(update) => {
                    let _ = updates_tx.send(update);
                },
                Err(error) => {
                    log::warn!("{}", error);
                    break;
                },
            },
            recv = rx.recv() => match recv {
                Some((callback, action)) => match action {
                    FernbedienungAction::SetCameraStream(enable) => {
                        cameras_stream.clear();
                        if enable {
                            for &(camera, width, height, port) in DRONE_CAMERAS_CONFIG {
                                let stream = MjpegStreamerStream::new(&device, camera, width, height, port);
                                let stream = tokio_stream::StreamExt::throttle(stream, Duration::from_millis(200));
                                cameras_stream.insert(camera.to_owned(), Box::pin(stream));
                            }
                        }
                        let _ = callback.send(Ok(()));
                    },
                    FernbedienungAction::Halt => {
                        let result = device.halt().await
                            .context("Could not halt Up Core");
                        let _ = callback.send(result);
                    },
                    FernbedienungAction::Reboot => {
                        let result = device.reboot().await
                            .context("Could not reboot Up Core");
                        let _ = callback.send(result);
                    },
                    FernbedienungAction::Bash(action) => {
                        if let Err(mpsc::error::SendError((callback, action))) = bash_tx.send((callback, action)).await {
                            let _ = callback.send(Err(anyhow::anyhow!("Could not send {:?} to Bash terminal", action)));
                        }
                    },
                    FernbedienungAction::UploadToTemporaryPath(files) => {
                        todo!()
                    }
                    FernbedienungAction::GetKernelMessages => {},
                    FernbedienungAction::Identify => {},
                },
                None => break,
            },
            _ = &mut bash_task => {
                /* restart task */
                let (tx, rx) = mpsc::channel(8);
                bash_tx = tx;
                bash_task.set(bash(&device, rx, updates_tx.clone()));
            },
            // _ = &mut experiment_task => {
            //     /* restart task */
            //     let (tx, rx) = mpsc::channel(8);
            //     experiment_tx = tx;
            //     //experiment_task.set(experiment(&device, rx, updates_tx.clone()));
            // },
        }
    }
}

pub async fn new(mut action_rx: Receiver) {
    /* fernbedienung task state */
    let fernbedienung_task = futures::future::pending().left_future();
    let mut fernbedienung_tx = Option::default();
    let mut fernbedienung_addr = Option::default();
    tokio::pin!(fernbedienung_task);
    /* xbee task state */
    let xbee_task = futures::future::pending().left_future();
    let mut xbee_tx = Option::default();
    let mut xbee_addr = Option::default();
    tokio::pin!(xbee_task);
    /* updates_tx is for sending changes in state to subscribers (e.g., the webui) */
    let (updates_tx, _) = broadcast::channel(16);
    /* path to the most recently uploaded experiment */
    let mut experiment_path: Option<String> = None;

    // TODO: for a clean shutdown we may want to consider the case where updates_tx hangs up
    loop {
        tokio::select! {
            Some(action) = action_rx.recv() => match action {
                Action::AssociateFernbedienung(device) => {
                    let (tx, rx) = mpsc::channel(8);
                    fernbedienung_tx = Some(tx);
                    fernbedienung_addr = Some(device.addr);
                    let _ = updates_tx.send(Update::FernbedienungConnected(device.addr));
                    fernbedienung_task.set(fernbedienung(device, rx, updates_tx.clone()).right_future());
                },
                Action::AssociateXbee(device) => {
                    let (tx, rx) = mpsc::channel(8);
                    xbee_tx = Some(tx);
                    xbee_addr = Some(device.addr);
                    let _ = updates_tx.send(Update::XbeeConnected(device.addr));
                    xbee_task.set(xbee(device, rx, updates_tx.clone()).right_future());
                },
                Action::ExecuteXbeeAction(callback, action) => match xbee_tx.as_ref() {
                    Some(tx) => {
                        let _ = tx.send((callback, action)).await;
                    },
                    None => {
                        let error = anyhow::anyhow!("Could not execute {:?}: Xbee is not connected.", action);
                        let _ = callback.send(Err(error));
                    }
                },
                Action::ExecuteFernbedienungAction(callback, action) => match fernbedienung_tx.as_ref() {
                    Some(tx) => {
                        let _ = tx.send((callback, action)).await;
                    },
                    None => {
                        let error = anyhow::anyhow!("Could not execute {:?}: Fernbedienung is not connected.", action);
                        let _ = callback.send(Err(error));
                    }
                },
                Action::Subscribe(callback) => {
                    /* note that upon subscribing all updates should be sent to ensure
                       that new clients are in sync */
                    if let Ok(_) = callback.send(updates_tx.subscribe()) {
                        if let Some(addr) = xbee_addr {
                            let _ = updates_tx.send(Update::XbeeConnected(addr));
                        }
                        if let Some(addr) = fernbedienung_addr {
                            let _ = updates_tx.send(Update::FernbedienungConnected(addr));
                        }
                    }
                },
                Action::UploadExperiment(callback, software) => match fernbedienung_tx.as_ref() {
                    Some(tx) => {
                        tx.send((callback, FernbedienungAction::UploadToTemporaryPath(software.0))).await;
                    }
                    None => {}
                },
                Action::StartExperiment(callback) => {

                },
                Action::StopExperiment => {
                    
                },
            },
            _ = &mut fernbedienung_task => {
                fernbedienung_tx = None;
                fernbedienung_addr = None;
                fernbedienung_task.set(futures::future::pending().left_future());
                let _ = updates_tx.send(Update::FernbedienungDisconnected);
            },
            result = &mut xbee_task => {
                xbee_tx = None;
                xbee_addr = None;
                xbee_task.set(futures::future::pending().left_future());
                let _ = updates_tx.send(Update::XbeeDisconnected);
                if let Err(error) = result {
                    log::warn!("{}", error);
                }
            }
        }
    }
}