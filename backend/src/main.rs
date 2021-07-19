use std::{net::SocketAddr, path::{Path, PathBuf}};
use ipnet::Ipv4Net;
use macaddr::{MacAddr6, MacAddr8};
use tokio::sync::mpsc;
use warp::Filter;
use structopt::StructOpt;
use anyhow::Context;

mod arena;
mod robot;
mod network;
mod webui;
mod optitrack;
mod software;
mod journal;
mod router;

// supervisor changes
// - configuration comes from XML (done)
// - network module just searches for fernbedienung/xbee instances and sends them to the arena (with the mac address?).
// - network module sends Ferbedienung(Macaddr6, Ipv4Addr) or Xbee(Macaddr6, Ipv4Addr) to the arena
// - if the arena does not have a robot associated with that Mac, a warning is printed and the address is no longer probed.
// - how do addresses get back to the network module? oneshot channel?
// - arena creates drones and pipucks as specified in the XML. These actor are initialised without connections. Requests are made over bounded channels to associate a fernbedienung or xbee device.

#[derive(Debug, StructOpt)]
#[structopt(name = "supervisor", about = "A supervisor for experiments with swarms of robots")]
struct Options {
    #[structopt(short = "c", long = "configuration")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    /* initialize the logger */
    let environment = env_logger::Env::default().default_filter_or("supervisor=info");
    env_logger::Builder::from_env(environment).format_timestamp_millis().init();
    /* parse the configuration file */
    let options = Options::from_args();
    let config = parse_config(&options.config)
        .context("Could not parse configuration file")?;

    log::info!("Configuration = {:?}", config);
    /* create a task for tracking the robots and state of the experiment */
    let (arena_requests_tx, arena_requests_rx) = mpsc::channel(32);
    let (journal_requests_tx, journal_requests_rx) = mpsc::channel(32);
    /* listen for the ctrl-c shutdown signal */
    let sigint_task = tokio::signal::ctrl_c();
    /* create journal task */
    let journal_task = journal::new(journal_requests_rx);
    /* create arena task */
    let arena_task = arena::new(arena_requests_rx, &journal_requests_tx);
    /* create network task */
    let network_task = network::new(config.robot_network, &arena_requests_tx);
    /* create message router task */
    let router_addr = config.router_socket
        .ok_or(anyhow::anyhow!("A socket for the message router must be provided"))?;
    let router_task = router::new(router_addr, journal_requests_tx.clone());
    /* create webui task */
    /* clone arena requests tx for moving into the closure */
    let arena_requests_tx = arena_requests_tx.clone();
    let arena_channel = warp::any().map(move || arena_requests_tx.clone());
    let socket_route = warp::path("socket")
        .and(warp::ws())
        .and(arena_channel)
        .map(|websocket: warp::ws::Ws, arena_requests_tx| {
            websocket.on_upgrade(move |socket| webui::run(socket, arena_requests_tx))
        });
    let static_route = warp::get()
        .and(static_dir::static_dir!("static"));
    //    .and(warp::fs::dir("/home/mallwright/Workspace/mns-supervisor/static"));
    let webui_addr = config.webui_socket
        .ok_or(anyhow::anyhow!("A socket for the message router must be provided"))?;
    let webui_task = warp::serve(socket_route.or(static_route)).run(webui_addr);
    /* pin the futures so that they can be polled via &mut */
    tokio::pin!(arena_task);
    tokio::pin!(journal_task);
    tokio::pin!(network_task);
    tokio::pin!(webui_task);
    tokio::pin!(sigint_task);

    let mut router_task = tokio::spawn(router_task);
    /* no point in implementing automatic browser opening */
    /* https://bugzilla.mozilla.org/show_bug.cgi?id=1512438 */
    let server_addr = format!("http://{}/", webui_addr);
    if let Err(_) = webbrowser::open(&server_addr) {
        log::warn!("Could not start browser");
        log::info!("Please open this URL manually: {}", server_addr);
    };
    
    tokio::select! {
        _ = &mut arena_task => {},
        _ = &mut journal_task => {},
        _ = &mut network_task => {},
        _ = &mut router_task => {},
        _ = &mut webui_task => {},
        _ = &mut sigint_task => {
            /* TODO: is it safe to do this? should messages be broadcast to robots */
            /* what happens if ARGoS is running on the robots, does breaking the
            connection to fernbedienung kill ARGoS? How does the Pixhawk respond */
            log::info!("Shutting down");
        }
    }

    Ok(())
}

/*
<?xml version="1.0" ?>
<configuration>
    <supervisor>
        <router socket="localhost:1234" />
        <webui socket="localhost:8000" />
    </supervisor>
    <robots network="192.168.1.0/24">
        <drone  id="drone1" 
                xbee_addr="FFFFFFFFFFFF"
                upcore_addr="FFFFFFFFFFFF"
                optitrack_id="1" />
        <pipuck id="pipuck1" 
                rpi_addr="FFFFFFFFFFFF"
                optitrack_id="2"
                apriltag_id="20" />
    </robots>
</configuration>

 */

#[derive(Debug)]
enum RobotTableEntry {
    Drone {
        id: String,
        xbee_macaddr: MacAddr8,
        upcore_macaddr: MacAddr6,
        optitrack_id: i32,
    },
    PiPuck {
        id: String,
        rpi_macaddr: MacAddr6,
        optitrack_id: i32,
        apriltag_id: u8,
    }
}

#[derive(Debug)]
struct Configuration {
    router_socket: Option<SocketAddr>,
    webui_socket: Option<SocketAddr>,
    robot_network: Ipv4Net,
    robot_table: Vec<RobotTableEntry>,
}

fn parse_config(config: &Path) -> anyhow::Result<Configuration> {
    let config = std::fs::read_to_string(config)?;
    let tree = roxmltree::Document::parse(&config)?;
    let configuration = tree
        .descendants()
        .find(|node| node.tag_name().name() == "configuration")
        .ok_or(anyhow::anyhow!("Could not find node <configuration>"))?;
    let supervisor = configuration
        .descendants()
        .find(|node| node.tag_name().name() == "supervisor")
        .ok_or(anyhow::anyhow!("Could not find node <supervisor>"))?;
    let webui_socket = supervisor
        .descendants()
        .find(|node| node.tag_name().name() == "webui")
        .map(|node| node
            .attribute("socket")
            .ok_or(anyhow::anyhow!("Could not find attribute \"socket\" in <webui>"))?
            .parse::<SocketAddr>()
            .context("Could not parse attribute \"socket\" in <webui>"))
        .transpose()?;
    let router_socket = supervisor
        .descendants()
        .find(|node| node.tag_name().name() == "router")
        .map(|node| node
            .attribute("socket")
            .ok_or(anyhow::anyhow!("Could not find attribute \"socket\" in <router>"))?
            .parse::<SocketAddr>()
            .context("Could not parse attribute \"socket\" in <router>"))
        .transpose()?;
    let robots = configuration
        .descendants()
        .find(|node| node.tag_name().name() == "robots")
        .ok_or(anyhow::anyhow!("Could not find node \"robots\" in <configuration>"))?;
    let robot_network = robots
        .attribute("network")
        .ok_or(anyhow::anyhow!("Could not find attribute \"network\" in <robots>"))?
        .parse::<Ipv4Net>()
        .context("Could not parse attribute \"network\" in <robots>")?;
    let mut robot_table = Vec::new();
    for robot in robots.descendants() {
        match robot.tag_name().name() {
            "pipuck" => {
                let pipuck = RobotTableEntry::PiPuck {
                    id: robot
                        .attribute("id")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"id\" for <pipuck>"))?
                        .to_owned(),
                    rpi_macaddr: robot.attribute("rpi_macaddr")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"rpi_macaddr\" for <pipuck>"))?
                        .parse()
                        .context("Could not parse attribute \"rpi_macaddr\" for <pipuck>")?,
                    optitrack_id: robot.attribute("optitrack_id")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"optitrack_id\" for <pipuck>"))?
                        .parse()
                        .context("Could not parse attribute \"optitrack_id\" for <pipuck>")?,
                    apriltag_id: robot.attribute("apriltag_id")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"apriltag_id\" for <pipuck>"))?
                        .parse()
                        .context("Could not parse attribute \"apriltag_id\" for <pipuck>")?,
                };
                robot_table.push(pipuck);
            },
            "drone" => {
                let drone = RobotTableEntry::Drone {
                    id: robot
                        .attribute("id")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"id\" for <drone>"))?
                        .to_owned(),
                    xbee_macaddr: robot.attribute("xbee_macaddr")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"xbee_macaddr\" for <drone>"))?
                        .parse()
                        .context("Could not parse attribute \"xbee_macaddr\" for <drone>")?,
                    upcore_macaddr: robot.attribute("upcore_macaddr")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"upcore_macaddr\" for <drone>"))?
                        .parse()
                        .context("Could not parse attribute \"upcore_macaddr\" for <drone>")?,
                    optitrack_id: robot.attribute("optitrack_id")
                        .ok_or(anyhow::anyhow!("Could not find attribute \"optitrack_id\" for <drone>"))?
                        .parse()
                        .context("Could not parse attribute \"optitrack_id\" for <drone>")?,
                };
                robot_table.push(drone);
            },
            _ => continue,
        }
    }
    Ok(Configuration { 
        router_socket,
        webui_socket,
        robot_network,
        robot_table,
    })
}