use std::{net::SocketAddr, path::{Path, PathBuf}};
use ipnet::Ipv4Net;
use structopt::StructOpt;
use anyhow::Context;
use tokio::sync::{broadcast, mpsc};

mod arena;
mod robot;
mod network;
mod webui;
mod optitrack;
mod journal;
mod router;

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
    let Configuration {
        router_socket,
        webui_socket,
        robot_network,
        pipucks,
        drones,
    } = parse_config(&options.config)
            .context(format!("Could not parse configuration file {:?}", options.config))?;
    /* create a task for tracking the robots and state of the experiment */
    let (arena_requests_tx, arena_requests_rx) = mpsc::channel(8);
    let (journal_requests_tx, journal_requests_rx) = mpsc::channel(8);
    /* listen for the ctrl-c shutdown signal */
    let sigint_task = tokio::signal::ctrl_c();
    /* create journal task */
    let journal_task = journal::new(journal_requests_rx);
    /* create arena task */
    let arena_task =
        arena::new(arena_requests_rx,
                   &journal_requests_tx,
                   pipucks,
                   drones);
    /* create network task */
    let network_task = network::new(robot_network, &arena_requests_tx);
    /* create message router task */
    let router_addr = router_socket
        .ok_or(anyhow::anyhow!("A socket for the message router must be provided"))?;
    let router_task = router::new(router_addr, journal_requests_tx.clone());
    /* create the backend task */
    let webui_socket = webui_socket
        .ok_or(anyhow::anyhow!("A socket for the web interface must be provided"))?;
    let webui_task = webui::run(webui_socket, arena_requests_tx.clone());
    /* pin the futures so that they can be polled via &mut */
    tokio::pin!(arena_task);
    tokio::pin!(journal_task);
    tokio::pin!(network_task);
    tokio::pin!(webui_task);
    tokio::pin!(sigint_task);
    tokio::pin!(router_task);
    /* no point in implementing automatic browser opening */
    /* https://bugzilla.mozilla.org/show_bug.cgi?id=1512438 */
    let server_addr = format!("http://{}/", webui_socket);
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

#[derive(Debug)]
struct Configuration {
    router_socket: Option<SocketAddr>,
    webui_socket: Option<SocketAddr>,
    robot_network: Ipv4Net,
    pipucks: Vec<robot::pipuck::Descriptor>,
    drones: Vec<robot::drone::Descriptor>,
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
    let pipucks = robots
        .descendants()
        .filter(|node| node.tag_name().name() == "pipuck")
        .map(|node| anyhow::Result::<_>::Ok(robot::pipuck::Descriptor {
            id: node.attribute("id")
                .ok_or(anyhow::anyhow!("Could not find attribute \"id\" for <pipuck>"))?
                .to_owned(),
            rpi_macaddr: node.attribute("rpi_macaddr")
                .ok_or(anyhow::anyhow!("Could not find attribute \"rpi_macaddr\" for <pipuck>"))?
                .parse()
                .context("Could not parse attribute \"rpi_macaddr\" for <pipuck>")?,
            optitrack_id: node.attribute("optitrack_id")
                .map(|value| value.parse())
                .transpose()
                .context("Could not parse attribute \"optitrack_id\" for <pipuck>")?,
            apriltag_id: node.attribute("apriltag_id")
                .map(|value| value.parse())
                .transpose()
                .context("Could not parse attribute \"apriltag_id\" for <pipuck>")?,
        }))
        .collect::<Result<Vec<_>, _>>()?;
    let drones = robots
        .descendants()
        .filter(|node| node.tag_name().name() == "drone")
        .map(|node| anyhow::Result::<_>::Ok(robot::drone::Descriptor {
            id: node.attribute("id")
                .ok_or(anyhow::anyhow!("Could not find attribute \"id\" for <drone>"))?
                .to_owned(),
            xbee_macaddr: node.attribute("xbee_macaddr")
                .ok_or(anyhow::anyhow!("Could not find attribute \"xbee_macaddr\" for <drone>"))?
                .parse()
                .context("Could not parse attribute \"xbee_macaddr\" for <drone>")?,
            upcore_macaddr: node.attribute("upcore_macaddr")
                .ok_or(anyhow::anyhow!("Could not find attribute \"upcore_macaddr\" for <drone>"))?
                .parse()
                .context("Could not parse attribute \"upcore_macaddr\" for <drone>")?,                
            optitrack_id: node.attribute("optitrack_id")
                .map(|value| value.parse())
                .transpose()
                .context("Could not parse attribute \"optitrack_id\" for <pipuck>")?,
        }))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Configuration { 
        router_socket,
        webui_socket,
        robot_network,
        pipucks,
        drones,
    })
}