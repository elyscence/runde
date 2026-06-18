#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::{Arc, Mutex};

use eframe::egui;
use iroh::{Endpoint, EndpointAddr, RelayMode, endpoint::presets};
use iroh_tickets::endpoint::EndpointTicket;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
};

const ALPN: &[u8] = b"runde/tcp-proxy/0";
const BUF: usize = 65536;

#[derive(Clone, PartialEq)]
enum Status {
    Idle,
    HostStarting,
    HostWaiting { ticket: String },
    HostConnected { ticket: String, peers: Vec<String> },
    JoinConnecting,
    JoinConnected,
    Error(String),
}

struct TunnelState {
    status: Status,
    stop_tx: Option<broadcast::Sender<()>>,
}

impl TunnelState {
    fn new() -> Self {
        Self {
            status: Status::Idle,
            stop_tx: None,
        }
    }

    fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        self.status = Status::Idle;
    }
}

#[derive(PartialEq)]
enum Tab {
    Host,
    Join,
}

struct App {
    tab: Tab,
    mc_port: String,
    ticket_in: String,
    local_port: String,
    state: Arc<Mutex<TunnelState>>,
    rt: tokio::runtime::Runtime,
    copied: bool,
    copy_timer: f64,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            tab: Tab::Host,
            mc_port: "25565".into(),
            ticket_in: String::new(),
            local_port: "25565".into(),
            state: Arc::new(Mutex::new(TunnelState::new())),
            rt: tokio::runtime::Runtime::new().unwrap(),
            copied: false,
            copy_timer: 0.0,
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.copied {
            self.copy_timer -= ui.ctx().input(|i| i.stable_dt) as f64;
            if self.copy_timer <= 0.0 {
                self.copied = false;
            }
            ui.ctx().request_repaint();
        }

        let status = self.state.lock().unwrap().status.clone();

        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.add_space(16.0);

            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.heading("Runde");
                ui.add_space(8.0);
            });

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);

            match &status {
                Status::Idle | Status::Error(_) => {
                    self.show_form(ui, &status);
                }
                Status::HostStarting => {
                    self.show_waiting(ui, "Запуск…");
                }
                Status::HostWaiting { ticket } => {
                    self.show_host_waiting(ui, ticket.clone());
                }
                Status::HostConnected { peers, .. } => {
                    let msg = format!(
                        "Друзья подключены ({})!\n\n{}",
                        peers.len(),
                        peers
                            .iter()
                            .map(|p| format!("• {}", &p[..p.len().min(16)]))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    self.show_connected(ui, msg);
                }
                Status::JoinConnecting => {
                    self.show_waiting(ui, "Подключение к хосту…");
                }
                Status::JoinConnected => {
                    self.show_connected(
                        ui,
                        format!(
                            "Подключено!\n\nОткрой Minecraft Add Server\n localhost:{}",
                            self.local_port
                        ),
                    );
                }
            }
        });
    }
}

impl App {
    fn show_form(&mut self, ui: &mut egui::Ui, status: &Status) {
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.selectable_value(&mut self.tab, Tab::Host, "  Host  ");
            ui.selectable_value(&mut self.tab, Tab::Join, "  Join  ");
        });

        ui.add_space(12.0);

        match self.tab {
            Tab::Host => {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.vertical(|ui| {
                        ui.label("Порт Minecraft-сервера:");
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.mc_port)
                                .desired_width(80.0)
                                .hint_text("25565"),
                        );
                        ui.add_space(10.0);

                        let valid = self.mc_port.parse::<u16>().is_ok();
                        ui.add_enabled_ui(valid, |ui| {
                            if ui.button("   Запустить хост  ").clicked() {
                                self.start_host();
                            }
                        });
                    });
                });
            }

            Tab::Join => {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.vertical(|ui| {
                        ui.label("Ticket от хоста:");
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::multiline(&mut self.ticket_in)
                                .desired_width(420.0)
                                .desired_rows(3)
                                .hint_text("endpoint..."),
                        );
                        ui.add_space(8.0);

                        ui.label("Локальный порт:");
                        ui.add_space(4.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.local_port)
                                .desired_width(80.0)
                                .hint_text("25565"),
                        );
                        ui.add_space(10.0);

                        let valid = !self.ticket_in.trim().is_empty()
                            && self.local_port.parse::<u16>().is_ok();

                        ui.add_enabled_ui(valid, |ui| {
                            if ui.button("  Подключиться  ").clicked() {
                                self.start_join();
                            }
                        });
                    });
                });
            }
        }

        if let Status::Error(msg) = status {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 80),
                    format!("Error  {msg}"),
                );
            });
        }

        let is_running = !matches!(status, Status::Idle | Status::Error(_));
        if is_running {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                if ui.button("Остановить").clicked() {
                    self.state.lock().unwrap().stop();
                    ui.ctx().request_repaint();
                }
            });
        }
    }

    fn show_waiting(&self, ui: &mut egui::Ui, msg: &str) {
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.spinner();
            ui.add_space(8.0);
            ui.label(msg);
        });
    }

    fn show_host_waiting(&mut self, ui: &mut egui::Ui, ticket: String) {
        let cmd = format!("{ticket}");
        let mut cmd_text = cmd.clone();

        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("Скинь другу эту команду:")
                        .color(egui::Color32::GRAY)
                        .size(12.0),
                );
                ui.add_space(6.0);

                egui::ScrollArea::horizontal()
                    .id_salt("ticket_scroll")
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut cmd_text)
                                .desired_width(440.0)
                                .desired_rows(2)
                                .font(egui::TextStyle::Monospace)
                                .interactive(false),
                        );
                    });

                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    let copy_label = if self.copied {
                        "  Скопировано  "
                    } else {
                        "  Скопировать  "
                    };
                    if ui.button(copy_label).clicked() && !self.copied {
                        ui.ctx().copy_text(cmd.clone());
                        self.copied = true;
                        self.copy_timer = 2.0;
                    }

                    ui.add_space(8.0);

                    if ui.button("  Стоп  ").clicked() {
                        self.state.lock().unwrap().stop();
                        ui.ctx().request_repaint();
                    }
                });

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add_space(6.0);
                    ui.colored_label(egui::Color32::GRAY, "Ожидаю подключения…");
                });
            });
        });
    }

    fn show_connected(&mut self, ui: &mut egui::Ui, msg: String) {
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(60, 180, 100),
                    egui::RichText::new("Подключено").size(16.0).strong(),
                );
                ui.add_space(8.0);
                for line in msg.lines() {
                    if line.is_empty() {
                        ui.add_space(4.0);
                    } else {
                        ui.label(line);
                    }
                }
                ui.add_space(12.0);
                if ui.button("  Остановить  ").clicked() {
                    self.state.lock().unwrap().stop();
                    ui.ctx().request_repaint();
                }
            });
        });
    }

    fn start_host(&self) {
        let port: u16 = match self.mc_port.parse() {
            Ok(p) => p,
            Err(_) => return,
        };

        let state = self.state.clone();
        let (stop_tx, _) = broadcast::channel::<()>(1);
        {
            let mut s = state.lock().unwrap();
            s.stop_tx = Some(stop_tx.clone());
            s.status = Status::HostStarting;
        }

        self.rt.spawn(async move {
            match run_host(state.clone(), stop_tx, port).await {
                Ok(_) => {}
                Err(e) => {
                    state.lock().unwrap().status = Status::Error(e.to_string());
                }
            }
        });
    }

    fn start_join(&self) {
        let ticket_str = self.ticket_in.trim().to_string();
        let local_port: u16 = match self.local_port.parse() {
            Ok(p) => p,
            Err(_) => return,
        };

        let state = self.state.clone();
        let (stop_tx, _) = broadcast::channel::<()>(1);
        {
            let mut s = state.lock().unwrap();
            s.stop_tx = Some(stop_tx.clone());
            s.status = Status::JoinConnecting;
        }

        self.rt.spawn(async move {
            match run_join(state.clone(), stop_tx, ticket_str, local_port).await {
                Ok(_) => {}
                Err(e) => {
                    state.lock().unwrap().status = Status::Error(e.to_string());
                }
            }
        });
    }
}

async fn run_host(
    state: Arc<Mutex<TunnelState>>,
    stop_tx: broadcast::Sender<()>,
    mc_port: u16,
) -> anyhow::Result<()> {
    let endpoint = Endpoint::builder(presets::N0)
        .relay_mode(RelayMode::Default)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;

    let node_addr: EndpointAddr = endpoint.addr();
    let ticket = EndpointTicket::new(node_addr).to_string();

    state.lock().unwrap().status = Status::HostWaiting {
        ticket: ticket.clone(),
    };

    let mut stop_rx = stop_tx.subscribe();

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let incoming = match incoming {
                    Some(i) => i,
                    None => break,
                };

                let conn = match incoming.accept() {
                    Ok(c) => match c.await {
                        Ok(c) => c,
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                };

                let peer = conn.remote_id().fmt_short().to_string();

                {
                    let mut s = state.lock().unwrap();
                    match &mut s.status {
                        Status::HostWaiting { ticket } => {
                            let ticket = ticket.clone();
                            s.status = Status::HostConnected { ticket, peers: vec![peer.clone()] };
                        }
                        Status::HostConnected { peers, .. } => {
                            if !peers.contains(&peer) {
                                peers.push(peer.clone());
                            }
                        }
                        _ => {}
                    }
                }

                let state2 = state.clone();
                tokio::spawn(async move {
                    loop {
                        match conn.accept_bi().await {
                            Ok(streams) => {
                                tokio::spawn(proxy(streams, mc_port));
                            }
                            Err(_) => {
                                // Убираем только ЭТОГО пира, хост не трогаем
                                let mut s = state2.lock().unwrap();
                                if let Status::HostConnected { peers, ticket } = &mut s.status {
                                    peers.retain(|p| p != &peer);
                                    if peers.is_empty() {
                                        let ticket = ticket.clone();
                                        s.status = Status::HostWaiting { ticket };
                                    }
                                }
                                break;
                            }
                        }
                    }
                });
            }
            _ = stop_rx.recv() => { break; }
        }
    }

    endpoint.close().await;
    Ok(())
}

async fn proxy(
    (mut qs, mut qr): (iroh::endpoint::SendStream, iroh::endpoint::RecvStream),
    mc_port: u16,
) -> anyhow::Result<()> {
    let tcp = TcpStream::connect(("127.0.0.1", mc_port)).await?;
    let (mut tr, mut tw) = tcp.into_split();

    tokio::select! {
        _ = copy(&mut qr, &mut tw) => {}
        _ = copy(&mut tr, &mut qs) => {}
    }

    let _ = qs.finish();
    Ok(())
}

async fn run_join(
    state: Arc<Mutex<TunnelState>>,
    stop_tx: broadcast::Sender<()>,
    ticket_str: String,
    local_port: u16,
) -> anyhow::Result<()> {
    let ticket: EndpointTicket = ticket_str
        .parse()
        .map_err(|_| anyhow::anyhow!("Неверный формат ticket"))?;

    let node_addr: EndpointAddr = ticket.into();

    let endpoint = Endpoint::builder(presets::N0)
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    let conn = Arc::new(
        endpoint
            .connect(node_addr, ALPN)
            .await
            .map_err(|e| anyhow::anyhow!("Не удалось подключиться: {e}"))?,
    );

    state.lock().unwrap().status = Status::JoinConnected;

    let listener = TcpListener::bind(("127.0.0.1", local_port))
        .await
        .map_err(|_| anyhow::anyhow!("Порт {local_port} уже занят"))?;

    let mut stop_rx = stop_tx.subscribe();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, _) = match accepted {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let conn2 = conn.clone();
                tokio::spawn(async move {
                    let _ = forward_tcp(tcp, conn2).await;
                });
            }
            _ = stop_rx.recv() => { break; }
        }
    }

    endpoint.close().await;
    Ok(())
}

async fn forward_tcp(tcp: TcpStream, conn: Arc<iroh::endpoint::Connection>) -> anyhow::Result<()> {
    let (mut qs, mut qr) = conn.open_bi().await?;
    let (mut tr, mut tw) = tcp.into_split();

    tokio::select! {
        _ = copy(&mut qr, &mut tw) => {}
        _ = copy(&mut tr, &mut qs) => {}
    }

    let _ = qs.finish();
    Ok(())
}

async fn copy<R, W>(r: &mut R, w: &mut W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; BUF];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n]).await?;
    }
    w.flush().await?;
    Ok(())
}

fn main() -> eframe::Result {
    tracing_subscriber::fmt::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Runde")
            .with_inner_size([500.0, 320.0])
            .with_min_inner_size([400.0, 260.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Runde",
        options,
        Box::new(|cc| {
            let mut fonts = egui::FontDefinitions::default();

            fonts.font_data.insert(
                "monocraft".to_owned(),
                egui::FontData::from_static(include_bytes!("../assets/Monocraft.ttf")).into(),
            );

            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "monocraft".to_owned());

            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "monocraft".to_owned());

            cc.egui_ctx.set_fonts(fonts);

            Ok(Box::new(App::new(cc)))
        }),
    )
}
