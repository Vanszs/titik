mod app;
mod cli;
mod config;
mod controller;
mod dto;
mod model;
mod resources;
mod service;
mod view;

fn main() -> anyhow::Result<()> {
    let opts = cli::parse(std::env::args());
    app::run(opts)
}
