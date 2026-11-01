//! Wayon – eine ressourcensparende Websuchmaschine.
//!
//! Aufbau:
//! * [`crawler`]  – höflicher, asynchroner Web-Crawler (robots.txt, Sitemaps, Politeness).
//! * [`store`]    – kompakter, append-only Crawl-Speicher (zstd-komprimierte Records).
//! * [`index`]    – eigener invertierter Index (FST-Wörterbuch, bitgepackte Postings,
//!   Positionen, komprimierter Dokumentspeicher, PageRank).
//! * [`search`]   – Query-Parser, BM25F-Ranking mit Proximity, Phrasen, `site:`,
//!   Rechtschreibkorrektur, Snippets.
//! * [`server`]   – kleiner HTTP-Server mit Web-Oberfläche und JSON-API.

#![allow(clippy::type_complexity, clippy::needless_range_loop)]

pub mod codec;
pub mod crawler;
pub mod index;
pub mod search;
pub mod server;
pub mod store;
pub mod text;
pub mod urlnorm;
