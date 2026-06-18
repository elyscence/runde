# Runde

A small desktop application for sharing a Minecraft world with friends without port forwarding, VPNs, or a public IP.

Built with Rust, egui and Iroh.

## How it works

The host starts a local Minecraft server and generates a ticket.

Friends paste the ticket into Runde and connect directly through Iroh.

No account, registration, or dedicated server is required.

## Features

* Direct peer-to-peer connection
* No port forwarding
* No VPN required
* Simple desktop UI
* Single executable

## Usage

### Host

1. Start your Minecraft server.
2. Enter the server port (usually `25565`).
3. Click **Start Host**.
4. Send the generated ticket to your friends.

### Join

1. Copy the ticket from the host.
2. Paste it into Runde.
3. Click **Connect**.
4. Join `localhost:25565` in Minecraft.

## Build

```bash
cargo build --release
```

## Tech Stack

* Rust
* egui / eframe
* Iroh
* Tokio

## Why?

Most solutions for playing Minecraft with friends require opening ports, configuring routers, renting servers, or installing VPN software.

Runde focuses on the simplest possible workflow:

Generate a ticket → send it to a friend → play.
