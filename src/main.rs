#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod updater;

use std::sync::{Arc, Mutex};

use eframe::egui;
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl, endpoint::presets};
use iroh_relay::RelayConfig;
use iroh_relay::tls::CaTlsConfig;
use iroh_tickets::endpoint::EndpointTicket;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::broadcast,
};
use tokio_stream::StreamExt as _;

const ALPN: &[u8] = b"runde/tcp-proxy/0";
const BUF: usize = 65536;

const CUSTOM_RELAY_URL: &str = env!("CUSTOM_RELAY_URL");

fn build_relay_mode() -> RelayMode {
    if CUSTOM_RELAY_URL.is_empty() {
        return RelayMode::Default;
    }
    let url: RelayUrl = CUSTOM_RELAY_URL
        .parse()
        .expect("CUSTOM_RELAY_URL: невалидный URL");

    let node = RelayConfig::new(url, None);
    let map = RelayMap::from_iter([node]);
    RelayMode::Custom(map)
}

const BASE_W: f32 = 560.0;
const BASE_H: f32 = 380.0;
const MIN_SCALE: f32 = 0.85;
const MAX_SCALE: f32 = 1.25;
const BTN_H: f32 = 28.0;
const INPUT_W: f32 = 120.0;

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

#[derive(Clone, PartialEq)]
enum UpdateState {
    Idle,
    Checking,
    Available(updater::UpdateInfo),
    Downloading,
    Installed,
    Error(String),
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
    centered: bool,
    update_state: Arc<Mutex<UpdateState>>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
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

        let rt = tokio::runtime::Runtime::new().unwrap();
        let update_state = Arc::new(Mutex::new(UpdateState::Idle));

        {
            let update_state = update_state.clone();
            let ctx = cc.egui_ctx.clone();
            *update_state.lock().unwrap() = UpdateState::Checking;
            rt.spawn(async move {
                let result = updater::check_for_update().await;
                let mut s = update_state.lock().unwrap();
                *s = match result {
                    Ok(Some(info)) => UpdateState::Available(info),
                    Ok(None) => UpdateState::Idle,
                    Err(e) => {
                        tracing::warn!(error = %e, "update check failed");
                        UpdateState::Idle
                    }
                };
                ctx.request_repaint();
            });
        }

        Self {
            tab: Tab::Host,
            mc_port: "25565".into(),
            ticket_in: String::new(),
            local_port: "25565".into(),
            state: Arc::new(Mutex::new(TunnelState::new())),
            rt,
            copied: false,
            copy_timer: 0.0,
            centered: false,
            update_state,
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        if self.copied {
            self.copy_timer -= ctx.input(|i| i.stable_dt) as f64;
            if self.copy_timer <= 0.0 {
                self.copied = false;
            }
            ctx.request_repaint();
        }

        if !self.centered {
            if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
                let cx = monitor.x * 0.5 - BASE_W * 0.5;
                let cy = monitor.y * 0.5 - BASE_H * 0.5;
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(cx, cy)));
                self.centered = true;
            }
        }

        egui::CentralPanel::default().show_inside(ui, |ui| {
            let avail_w = ui.available_width().max(1.0);
            let scale = (avail_w / BASE_W).clamp(MIN_SCALE, MAX_SCALE);

            {
                let mut style = (*ctx.global_style()).clone();
                style.spacing.button_padding = egui::vec2(8.0 * scale, 4.0 * scale);
                style.spacing.item_spacing = egui::vec2(8.0 * scale, 8.0 * scale);
                ctx.set_global_style(style);
            }

            let body_size = 14.0 * scale;
            let mono_size = 12.0 * scale;

            let status = self.state.lock().unwrap().status.clone();

            self.show_update_banner(ui, scale, body_size);

            ui.add_space(16.0 * scale);
            ui.vertical_centered(|ui| {
                ui.heading(egui::RichText::new("Runde").size(23.0 * scale).strong());
            });
            ui.add_space(12.0 * scale);
            ui.separator();
            ui.add_space(14.0 * scale);

            match &status {
                Status::Idle | Status::Error(_) => self.show_form(ui, &status, scale, body_size),
                Status::HostStarting => self.show_waiting(ui, "Запуск…", scale, body_size),
                Status::HostWaiting { ticket } => {
                    self.show_host_waiting(ui, ticket.clone(), scale, body_size, mono_size)
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
                    self.show_connected(ui, msg, scale, body_size);
                }
                Status::JoinConnecting => {
                    self.show_waiting(ui, "Подключение к хосту…", scale, body_size)
                }
                Status::JoinConnected => {
                    let msg = format!("Подключено!\nЛокальный порт: {}", self.local_port);
                    self.show_connected(ui, msg, scale, body_size);
                }
            }
        });
    }
}

impl App {
    fn tab_btn(
        ui: &mut egui::Ui,
        text: &str,
        selected: bool,
        scale: f32,
        body_size: f32,
    ) -> egui::Response {
        let fill = if selected {
            egui::Color32::from_rgb(20, 115, 170)
        } else {
            ui.visuals().widgets.inactive.weak_bg_fill
        };
        let stroke = if selected {
            egui::Stroke::new(1.0, egui::Color32::from_rgb(35, 145, 210))
        } else {
            ui.visuals().widgets.inactive.bg_stroke
        };
        ui.add_sized(
            [84.0 * scale, 24.0 * scale],
            egui::Button::new(egui::RichText::new(text).size(body_size))
                .fill(fill)
                .stroke(stroke),
        )
    }

    fn show_form(&mut self, ui: &mut egui::Ui, status: &Status, scale: f32, body_size: f32) {
        let pad = 18.0 * scale;
        let ticket_w = ((BASE_W - 72.0) * scale).max(280.0 * scale);

        ui.horizontal(|ui| {
            ui.add_space(pad);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let host_clicked =
                        Self::tab_btn(ui, "Host", self.tab == Tab::Host, scale, body_size)
                            .clicked();
                    let join_clicked =
                        Self::tab_btn(ui, "Join", self.tab == Tab::Join, scale, body_size)
                            .clicked();
                    if host_clicked {
                        self.tab = Tab::Host;
                    }
                    if join_clicked {
                        self.tab = Tab::Join;
                    }
                });

                ui.add_space(16.0 * scale);

                match self.tab {
                    Tab::Host => {
                        ui.label(egui::RichText::new("Порт Minecraft-сервера:").size(body_size));
                        ui.add_space(4.0 * scale);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.mc_port)
                                .desired_width(INPUT_W * scale)
                                .font(egui::FontId::proportional(body_size))
                                .hint_text("25565"),
                        );
                        ui.add_space(12.0 * scale);

                        let valid = self.mc_port.parse::<u16>().is_ok();
                        let btn = ui.add_enabled(
                            valid,
                            egui::Button::new(
                                egui::RichText::new("Запустить хост").size(body_size),
                            ),
                        );
                        if btn.clicked() {
                            self.start_host();
                        }
                    }

                    Tab::Join => {
                        ui.label(egui::RichText::new("Ticket от хоста:").size(body_size));
                        ui.add_space(4.0 * scale);
                        ui.add_sized(
                            [ticket_w, 74.0 * scale],
                            egui::TextEdit::multiline(&mut self.ticket_in)
                                .desired_rows(3)
                                .font(egui::FontId::proportional(body_size))
                                .hint_text("endpoint..."),
                        );
                        ui.add_space(10.0 * scale);
                        ui.label(egui::RichText::new("Локальный порт:").size(body_size));
                        ui.add_space(4.0 * scale);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.local_port)
                                .desired_width(INPUT_W * scale)
                                .font(egui::FontId::proportional(body_size))
                                .hint_text("25565"),
                        );
                        ui.add_space(12.0 * scale);

                        let valid = !self.ticket_in.trim().is_empty()
                            && self.local_port.parse::<u16>().is_ok();
                        let btn = ui.add_enabled(
                            valid,
                            egui::Button::new(egui::RichText::new("Подключиться").size(body_size)),
                        );
                        if btn.clicked() {
                            self.start_join();
                        }
                    }
                }

                if let Status::Error(msg) = status {
                    ui.add_space(12.0 * scale);
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 80, 80),
                        egui::RichText::new(format!("✗  {msg}")).size(body_size),
                    );
                }
            });
        });
    }

    fn show_update_banner(&mut self, ui: &mut egui::Ui, scale: f32, body_size: f32) {
        let state = self.update_state.lock().unwrap().clone();
        let small = body_size * 0.9;

        match state {
            UpdateState::Idle | UpdateState::Checking => {}
            UpdateState::Available(info) => {
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(35, 55, 75))
                    .inner_margin(egui::Margin::same((8.0 * scale) as i8))
                    .corner_radius((4.0 * scale) as u8)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Доступно обновление: v{}",
                                    info.version
                                ))
                                .size(small),
                            );
                            if ui
                                .add_sized(
                                    [90.0 * scale, 22.0 * scale],
                                    egui::Button::new(egui::RichText::new("Обновить").size(small)),
                                )
                                .clicked()
                            {
                                self.start_update(info.clone());
                            }
                        });
                    });
                ui.add_space(8.0 * scale);
            }
            UpdateState::Downloading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add_space(6.0 * scale);
                    ui.colored_label(
                        egui::Color32::GRAY,
                        egui::RichText::new("Скачивание обновления…").size(small),
                    );
                });
                ui.add_space(8.0 * scale);
            }
            UpdateState::Installed => {
                egui::Frame::default()
                    .fill(egui::Color32::from_rgb(35, 70, 45))
                    .inner_margin(egui::Margin::same((8.0 * scale) as i8))
                    .corner_radius((4.0 * scale) as u8)
                    .show(ui, |ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 220, 150),
                            egui::RichText::new("Обновление установлено. Перезапустите Runde.")
                                .size(small),
                        );
                    });
                ui.add_space(8.0 * scale);
            }
            UpdateState::Error(msg) => {
                ui.colored_label(
                    egui::Color32::from_rgb(220, 80, 80),
                    egui::RichText::new(format!("Ошибка обновления: {msg}")).size(small),
                );
                ui.add_space(8.0 * scale);
            }
        }
    }

    fn start_update(&self, info: updater::UpdateInfo) {
        let update_state = self.update_state.clone();
        *update_state.lock().unwrap() = UpdateState::Downloading;
        self.rt.spawn(async move {
            let result = updater::download_and_install(&info).await;
            let mut s = update_state.lock().unwrap();
            *s = match result {
                Ok(()) => UpdateState::Installed,
                Err(e) => {
                    tracing::error!(error = %e, "update install failed");
                    UpdateState::Error(e.to_string())
                }
            };
        });
    }

    fn show_waiting(&self, ui: &mut egui::Ui, msg: &str, scale: f32, body_size: f32) {
        ui.horizontal(|ui| {
            ui.add_space(18.0 * scale);
            ui.spinner();
            ui.add_space(8.0 * scale);
            ui.label(egui::RichText::new(msg).size(body_size));
        });
    }

    fn show_host_waiting(
        &mut self,
        ui: &mut egui::Ui,
        ticket: String,
        scale: f32,
        body_size: f32,
        mono_size: f32,
    ) {
        let mut display = ticket.clone();
        let ticket_w = ((BASE_W - 72.0) * scale).max(280.0 * scale);

        ui.horizontal(|ui| {
            ui.add_space(18.0 * scale);
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("Скинь другу:")
                        .color(egui::Color32::GRAY)
                        .size(body_size * 0.9),
                );
                ui.add_space(6.0 * scale);
                ui.add_sized(
                    [ticket_w, 62.0 * scale],
                    egui::TextEdit::multiline(&mut display)
                        .desired_rows(2)
                        .font(egui::FontId::monospace(mono_size))
                        .interactive(false),
                );
                ui.add_space(10.0 * scale);
                ui.horizontal(|ui| {
                    let copy_label = if self.copied {
                        "Скопировано"
                    } else {
                        "Скопировать"
                    };
                    if ui
                        .add_sized(
                            [150.0 * scale, BTN_H * scale],
                            egui::Button::new(egui::RichText::new(copy_label).size(body_size)),
                        )
                        .clicked()
                        && !self.copied
                    {
                        ui.ctx().copy_text(ticket.clone());
                        self.copied = true;
                        self.copy_timer = 2.0;
                    }
                    ui.add_space(8.0 * scale);
                    if ui
                        .add_sized(
                            [100.0 * scale, BTN_H * scale],
                            egui::Button::new(egui::RichText::new("Стоп").size(body_size)),
                        )
                        .clicked()
                    {
                        self.state.lock().unwrap().stop();
                        ui.ctx().request_repaint();
                    }
                });
                ui.add_space(10.0 * scale);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add_space(6.0 * scale);
                    ui.colored_label(
                        egui::Color32::GRAY,
                        egui::RichText::new("Ожидаю подключения…").size(body_size),
                    );
                });
            });
        });
    }

    fn show_connected(&mut self, ui: &mut egui::Ui, msg: String, scale: f32, body_size: f32) {
        ui.horizontal(|ui| {
            ui.add_space(18.0 * scale);
            ui.vertical(|ui| {
                ui.add_space(4.0 * scale);
                ui.colored_label(
                    egui::Color32::from_rgb(60, 180, 100),
                    egui::RichText::new("✓  Подключено")
                        .size(body_size * 1.15)
                        .strong(),
                );
                ui.add_space(8.0 * scale);
                for line in msg.lines() {
                    if line.is_empty() {
                        ui.add_space(4.0 * scale);
                    } else {
                        ui.label(egui::RichText::new(line).size(body_size));
                    }
                }

                ui.add_space(12.0 * scale);
                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [150.0 * scale, BTN_H * scale],
                            egui::Button::new(egui::RichText::new("Остановить").size(body_size)),
                        )
                        .clicked()
                    {
                        self.state.lock().unwrap().stop();
                        ui.ctx().request_repaint();
                    }
                    ui.add_space(8.0 * scale);
                    if ui
                        .add_sized(
                            [150.0 * scale, BTN_H * scale],
                            egui::Button::new(egui::RichText::new("Экспорт логов").size(body_size)),
                        )
                        .clicked()
                    {
                        match export_logs() {
                            Ok(path) => tracing::info!(path = %path.display(), "logs exported"),
                            Err(e) => tracing::error!(error = %e, "log export failed"),
                        }
                    }
                });
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
            if let Err(e) = run_host(state.clone(), stop_tx, port).await {
                state.lock().unwrap().status = Status::Error(e.to_string());
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
            if let Err(e) = run_join(state.clone(), stop_tx, ticket_str, local_port).await {
                state.lock().unwrap().status = Status::Error(e.to_string());
            }
        });
    }
}

async fn run_host(
    state: Arc<Mutex<TunnelState>>,
    stop_tx: broadcast::Sender<()>,
    mc_port: u16,
) -> anyhow::Result<()> {
    let mut endpoint_builder = Endpoint::builder(presets::N0)
        .relay_mode(build_relay_mode())
        .alpns(vec![ALPN.to_vec()]);
    endpoint_builder = endpoint_builder.ca_tls_config(CaTlsConfig::embedded());
    let endpoint = endpoint_builder.bind().await?;

    let node_addr: EndpointAddr = endpoint.addr();
    let ticket = EndpointTicket::new(node_addr).to_string();

    state.lock().unwrap().status = Status::HostWaiting {
        ticket: ticket.clone(),
    };

    let mut stop_rx = stop_tx.subscribe();

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let incoming = match incoming { Some(i) => i, None => break };
                let conn = match incoming.accept() {
                    Ok(c) => match c.await { Ok(c) => c, Err(_) => continue },
                    Err(_) => continue,
                };
                let peer = conn.remote_id().fmt_short().to_string();
                tracing::info!(peer = %peer, "peer connected");
                {
                    let mut s = state.lock().unwrap();
                    match &mut s.status {
                        Status::HostWaiting { ticket } => {
                            let t = ticket.clone();
                            s.status = Status::HostConnected { ticket: t, peers: vec![peer.clone()] };
                        }
                        Status::HostConnected { peers, .. } => {
                            if !peers.contains(&peer) { peers.push(peer.clone()); }
                        }
                        _ => {}
                    }
                }

                tokio::spawn(log_path_events(conn.clone(), peer.clone()));

                let state2 = state.clone();
                let ticket2 = ticket.clone();
                let peer2 = peer.clone();
                tokio::spawn(async move {
                    loop {
                        match conn.accept_bi().await {
                            Ok(streams) => { tokio::spawn(proxy(streams, mc_port)); }
                            Err(e) => {
                                tracing::warn!(peer = %peer2, error = %e, "peer connection lost");
                                let mut s = state2.lock().unwrap();
                                if let Status::HostConnected { peers, .. } = &mut s.status {
                                    peers.retain(|p| p != &peer2);
                                    if peers.is_empty() {
                                        s.status = Status::HostWaiting { ticket: ticket2.clone() };
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
    let _ = tw.shutdown().await;
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

    let mut endpoint_builder = Endpoint::builder(presets::N0).relay_mode(build_relay_mode());
    endpoint_builder = endpoint_builder.ca_tls_config(CaTlsConfig::embedded());
    let endpoint = endpoint_builder.bind().await?;

    let conn = Arc::new(
        endpoint
            .connect(node_addr, ALPN)
            .await
            .map_err(|e| anyhow::anyhow!("Не удалось подключиться: {e}"))?,
    );

    tracing::info!("connected to host");
    tokio::spawn(log_path_events((*conn).clone(), "host".to_string()));

    state.lock().unwrap().status = Status::JoinConnected;

    let listener = TcpListener::bind(("127.0.0.1", local_port))
        .await
        .map_err(|_| anyhow::anyhow!("Порт {local_port} уже занят"))?;

    let mut stop_rx = stop_tx.subscribe();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (tcp, _) = match accepted { Ok(v) => v, Err(_) => break };
                let conn2 = conn.clone();
                tokio::spawn(async move { let _ = forward_tcp(tcp, conn2).await; });
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
    let _ = tw.shutdown().await;
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
        let n = match r.read(&mut buf).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "stream read failed");
                return Err(e.into());
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = w.write_all(&buf[..n]).await {
            tracing::warn!(error = %e, "stream write failed");
            return Err(e.into());
        }
    }
    w.flush().await?;
    Ok(())
}

async fn log_path_events(conn: iroh::endpoint::Connection, label: String) {
    let mut events = conn.path_events();
    while let Some(event) = events.next().await {
        match event {
            iroh::endpoint::PathEvent::Opened {
                id, remote_addr, ..
            } => {
                tracing::info!(peer = %label, path_id = ?id, addr = ?remote_addr, "path opened");
            }
            iroh::endpoint::PathEvent::Selected {
                id, remote_addr, ..
            } => {
                tracing::warn!(peer = %label, path_id = ?id, addr = ?remote_addr, "path SELECTED (active route changed)");
            }
            iroh::endpoint::PathEvent::Closed { id, last_stats, .. } => {
                tracing::warn!(peer = %label, path_id = ?id, rtt = ?last_stats.rtt, "path CLOSED");
            }
            other => {
                tracing::debug!(peer = %label, event = ?other, "path event (unhandled variant)");
            }
        }
    }
    tracing::info!(peer = %label, "path event stream ended (connection closed)");
}

fn log_dir() -> std::path::PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("Runde").join("logs")
}

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);

    let file_appender = tracing_appender::rolling::daily(&dir, "runde.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,iroh=info"));

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(false)
        .init();

    guard
}

fn export_logs() -> anyhow::Result<std::path::PathBuf> {
    let dir = log_dir();
    let desktop = dirs::desktop_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let out_path = desktop.join(format!(
        "runde-logs-{}.zip",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ));

    let file = std::fs::File::create(&out_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();

    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.path().is_file() {
            zip.start_file(entry.file_name().to_string_lossy(), options)?;
            let content = std::fs::read(entry.path())?;
            std::io::Write::write_all(&mut zip, &content)?;
        }
    }
    zip.finish()?;
    Ok(out_path)
}

fn main() -> eframe::Result {
    let _log_guard = init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Runde started");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Runde")
            .with_inner_size([BASE_W, BASE_H])
            .with_min_inner_size([480.0, 320.0])
            .with_max_inner_size([840.0, 560.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native("Runde", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
