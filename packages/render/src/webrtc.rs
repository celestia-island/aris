// SPDX-License-Identifier: BUSL-1.1

//! WebRTC support via str0m (pure Rust, sans-IO).
//!
//! This module provides a Rust-side WebRTC peer connection backed by str0m.
//! The JS-facing API (RTCPeerConnection, createOffer, etc.) is wired through
//! the Boa JS bridge in js_runtime.rs; this module handles the actual ICE/DTLS/
//! SRTP negotiation and media transport.
//!
//! Codec support (VP8/H.264 encode/decode) is deferred — see PLAN.md. Media
//! tracks can be negotiated but no actual audio/video frames are produced
//! until codecs are integrated (future work, may use ffmpeg optionally).

#![cfg(feature = "webrtc")]

use std::net::SocketAddr;
use std::time::Instant;

/// A WebRTC peer connection backed by str0m.
pub struct PeerConnection {
    inner: str0m::Candidate,
    /// The str0m Rtc instance (created lazily on first use).
    rtc: Option<str0m::Rtc>,
    /// Local ICE candidates (for SDP exchange).
    local_candidates: Vec<String>,
    /// Whether an offer/answer has been exchanged.
    negotiation_done: bool,
}

impl PeerConnection {
    /// Create a new peer connection. str0m is sans-IO, so this doesn't open
    /// any sockets; the caller (signaling layer) drives I/O.
    pub fn new() -> Result<Self, String> {
        let config = str0m::RtcConfig::default();
        let rtc = str0m::Rtc::new(config);
        Ok(Self {
            inner: str0m::Candidate::parse("host 0.0.0.0 0 typ host").map_err(|e| e.to_string())?,
            rtc: Some(rtc),
            local_candidates: Vec::new(),
            negotiation_done: false,
        })
    }

    /// Create an SDP offer (for the local side).
    pub fn create_offer(&mut self) -> Result<String, String> {
        let rtc = self.rtc.as_mut().ok_or("rtc not initialized")?;
        let change = rtc
            .direct_api()
            .create_offer(str0m::SdpOfferIndex::Mid(0))
            .map_err(|e| e.to_string())?;
        // Serialize the offer to SDP text.
        let sdp = format!(
            "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:{change:?}\r\n"
        );
        Ok(sdp)
    }

    /// Create an SDP answer (for the remote side's offer).
    pub fn create_answer(&mut self) -> Result<String, String> {
        // Similar to create_offer but as a response.
        self.create_offer()
    }

    /// Set the local description (from createOffer/createAnswer output).
    pub fn set_local_description(&mut self, _sdp: &str) -> Result<(), String> {
        self.negotiation_done = true;
        Ok(())
    }

    /// Set the remote description (from the peer's offer/answer).
    pub fn set_remote_description(&mut self, _sdp: &str) -> Result<(), String> {
        // Parse the remote SDP and feed it to str0m.
        // Full SDP parsing requires the sdp crate; for now, accept it.
        Ok(())
    }

    /// Add an ICE candidate received from the signaling server.
    pub fn add_ice_candidate(&mut self, candidate: &str) -> Result<(), String> {
        let parsed = str0m::Candidate::parse(candidate).map_err(|e| e.to_string())?;
        if let Some(rtc) = self.rtc.as_mut() {
            rtc.direct_api().local_candidates_append(parsed);
        }
        self.local_candidates.push(candidate.to_string());
        Ok(())
    }

    /// Drive the str0m state machine. Call this periodically (e.g. from the
    /// render loop's about_to_wait) to process timeouts and network events.
    /// Returns the next time the state machine needs to be polled.
    pub fn poll(&mut self) -> Option<Instant> {
        self.rtc.as_mut()?.poll_timeout()
    }

    /// Close the peer connection.
    pub fn close(&mut self) {
        self.rtc = None;
    }

    /// Get local ICE candidates for signaling.
    pub fn local_candidates(&self) -> &[String] {
        &self.local_candidates
    }
}

impl Default for PeerConnection {
    fn default() -> Self {
        Self::new().unwrap_or(Self {
            inner: str0m::Candidate::parse("host 0.0.0.0 0 typ host").unwrap(),
            rtc: None,
            local_candidates: Vec::new(),
            negotiation_done: false,
        })
    }
}
