// src/server/client.rs
//! Client connection handling.
//!
//! This module handles individual client connections, including authentication,
//! session setup, and message processing.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
// Add missing import for error! macro
use futures::{SinkExt, StreamExt, channel::mpsc::UnboundedSender}; // Added UnboundedSender
use tokio::sync::{Mutex, RwLock}; // Added Mutex
use tokio::time;
use tokio_rustls::TlsAcceptor;
// Correctly import the error macro
use tracing::{debug, error, info, trace, warn};
use tokio_tungstenite::tungstenite::Message; // Import Message

use crate::auth::AuthManager;
use crate::crypto::{KeyManager, SessionKeyManager};
use crate::network::{IpPoolManager, NetworkMonitor};
use crate::protocol::PacketType;
use crate::protocol::serialization::{packet_to_ws_message, ws_message_to_packet, create_error_packet, create_disconnect_packet, log_packet_info};
// Remove unused import: SessionError
use crate::server::session::{ClientSession, SessionManager};
use crate::server::routing::PacketRouter;
use crate::server::metrics::ServerMetricsCollector;
use crate::server::core::{ServerError, ServerState};
use crate::utils::{current_timestamp_millis, random_string};
use crate::utils::security::StringValidator;
use solana_sdk::pubkey::Pubkey;


/// Handle a client connection
pub async fn handle_client(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    tls_acceptor: Arc<TlsAcceptor>,
    key_manager: Arc<KeyManager>,
    auth_manager: Arc<AuthManager>,
    ip_pool: Arc<IpPoolManager>,
    session_manager: Arc<SessionManager>,
    session_key_manager: Arc<SessionKeyManager>,
    network_monitor: Arc<NetworkMonitor>,
    packet_router: Arc<PacketRouter>,
    metrics: Arc<ServerMetricsCollector>,
    server_state: Arc<RwLock<ServerState>>,
) -> Result<(), ServerError> {
    // Record TLS handshake start in metrics
    metrics.record_handshake_start().await;

    // Perform TLS handshake
    let tls_stream = match tls_acceptor.accept(stream).await {
        Ok(stream) => {
            // Record successful handshake
            metrics.record_handshake_complete().await;
            debug!("TLS handshake successful with {}", addr);
            stream
        }
        Err(e) => {
            // Record failed handshake
            // TODO: Should this be recorded if the handshake *failed*?
            // metrics.record_handshake_complete().await;
            return Err(ServerError::Tls(format!("TLS handshake failed: {}", e)));
        }
    };

    // Upgrade connection to WebSocket
    let ws_stream = match tokio_tungstenite::accept_async(tls_stream).await {
        Ok(stream) => {
            debug!("WebSocket connection established with {}", addr);
            stream
        }
        Err(e) => {
            return Err(ServerError::WebSocket(e));
        }
    };

    // Split the WebSocket stream
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // --- Authentication Phase ---
    let public_key_string = match time::timeout(Duration::from_secs(30), ws_receiver.next()).await {
        Ok(Some(Ok(msg))) => {
             match ws_message_to_packet(&msg) {
                Ok(PacketType::Auth { public_key, version, features, nonce: _ }) => { // Mark nonce unused
                    debug!("Auth request from {}, version: {}, features: {:?}", public_key, version, features);

                    // Verify public key format
                    if !StringValidator::is_valid_solana_pubkey(&public_key) {
                        let error_packet = create_error_packet(1001, "Invalid public key format");
                        let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                        metrics.record_auth_failure().await;
                        return Err(ServerError::Authentication("Invalid public key format".to_string()));
                    }

                    // Generate challenge
                    let challenge = match auth_manager.generate_challenge(&addr.to_string()).await {
                        Ok(challenge) => challenge,
                        Err(e) => {
                             let error_packet = create_error_packet(1001, &format!("Failed to generate challenge: {}", e));
                            let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                            metrics.record_auth_failure().await;
                            return Err(ServerError::Authentication(format!("Challenge generation failed: {}", e)));
                        }
                    };

                    // Get server public key
                    let server_pubkey = key_manager.public_key().await.to_string();

                    // Create challenge packet
                    let challenge_packet = PacketType::Challenge {
                        data: challenge.1.clone(), // Challenge data
                        server_key: server_pubkey,
                        expires_at: current_timestamp_millis() + crate::config::constants::AUTH_CHALLENGE_TIMEOUT.as_millis() as u64,
                        id: challenge.0.clone(), // Challenge ID
                    };

                    // Send challenge
                    if ws_sender.send(packet_to_ws_message(&challenge_packet)?).await.is_err() {
                        return Err(ServerError::Network("Failed to send challenge".to_string()));
                    }

                    // Wait for challenge response
                    match time::timeout(Duration::from_secs(30), ws_receiver.next()).await {
                         Ok(Some(Ok(resp_msg))) => {
                             match ws_message_to_packet(&resp_msg) {
                                Ok(PacketType::ChallengeResponse { signature, public_key: resp_pubkey, challenge_id }) => {
                                    if resp_pubkey != public_key {
                                        let error_packet = create_error_packet(1001, "Public key mismatch");
                                        let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                                        metrics.record_auth_failure().await;
                                        return Err(ServerError::Authentication("Public key mismatch".to_string()));
                                    }

                                    // Verify the challenge
                                    match auth_manager.verify_challenge(&challenge_id, &signature, &public_key, &addr.to_string()).await {
                                        Ok(_) => {
                                            debug!("Challenge successfully verified for {}", public_key);
                                            if !auth_manager.is_client_allowed(&public_key).await {
                                                 let error_packet = create_error_packet(1005, "Access denied by ACL");
                                                let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                                                metrics.record_auth_failure().await;
                                                return Err(ServerError::Authentication("Access denied by ACL".to_string()));
                                            }
                                            metrics.record_auth_success().await;
                                            info!("Client {} authenticated successfully", public_key);
                                            public_key // Return the verified public key
                                        }
                                        Err(e) => {
                                             let error_packet = create_error_packet(1001, &format!("Challenge verification failed: {}", e));
                                            let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                                            metrics.record_auth_failure().await;
                                            return Err(ServerError::Authentication(format!("Challenge verification failed: {}", e)));
                                        }
                                    }
                                }
                                Ok(_) => {
                                    let error_packet = create_error_packet(1002, "Expected challenge response");
                                    let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                                    metrics.record_auth_failure().await;
                                    return Err(ServerError::Authentication("Expected challenge response".to_string()));
                                }
                                Err(e) => {
                                    let error_packet = create_error_packet(1002, &format!("Invalid challenge response message: {}", e));
                                     let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                                    metrics.record_auth_failure().await;
                                    return Err(ServerError::Protocol(e));
                                }
                            }
                        }
                        Ok(Some(Err(e))) | Err(_) /* Timeout or Error */ => {
                            metrics.record_auth_failure().await;
                            if let Ok(Some(Err(e))) = time::timeout(Duration::from_secs(1), ws_receiver.next()).await {
                                 return Err(ServerError::WebSocket(e));
                             }
                             return Err(ServerError::Authentication("WebSocket closed or timed out during challenge response".to_string()));
                        }
                         Ok(None) => { // Stream closed
                             metrics.record_auth_failure().await;
                             return Err(ServerError::Authentication("WebSocket closed during challenge response".to_string()));
                         }
                    }
                }
                 Ok(_) => { // Wrong initial packet type
                     let error_packet = create_error_packet(1002, "Expected authentication message");
                     let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                     metrics.record_auth_failure().await;
                     return Err(ServerError::Authentication("Expected authentication message".to_string()));
                 }
                 Err(e) => { // Deserialization error
                     let error_packet = create_error_packet(1002, &format!("Invalid auth message: {}", e));
                     let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
                     metrics.record_auth_failure().await;
                     return Err(ServerError::Protocol(e));
                 }
             }
        }
         Ok(Some(Err(e))) | Err(_) /* Timeout or Error */ => {
             metrics.record_auth_failure().await;
             if let Ok(Some(Err(e))) = time::timeout(Duration::from_secs(1), ws_receiver.next()).await {
                 return Err(ServerError::WebSocket(e));
             }
             return Err(ServerError::Authentication("WebSocket closed or timed out during authentication".to_string()));
         }
         Ok(None) => { // Stream closed
             metrics.record_auth_failure().await;
             return Err(ServerError::Authentication("WebSocket closed before authentication".to_string()));
         }
    };
    // --- Authentication Phase End ---


    // Assign IP address
    let ip_address = match ip_pool.allocate_ip(&public_key_string).await {
        Ok(ip) => {
            debug!("Assigned IP {} to client {}", ip, public_key_string);
            ip
        }
        Err(e) => {
            let error_packet = create_error_packet(1007, &format!("Failed to allocate IP: {}", e));
            let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
            return Err(ServerError::Network(format!("IP allocation failed: {}", e)));
        }
    };

    // Generate session ID
    let session_id = format!("session_{}", random_string(16));

    // Generate session key
    let session_key = SessionKeyManager::generate_key();

    // Store session key
    session_key_manager.store_key(&public_key_string, session_key.clone()).await;

    // Get shared secret for encrypting session key
    let pubkey = Pubkey::from_str(&public_key_string)
        .map_err(|e| ServerError::KeyError(format!("Invalid public key: {}", e)))?;
    let shared_secret = match key_manager.get_shared_secret(&pubkey).await {
        Ok(secret) => secret,
        Err(e) => {
            let error_packet = create_error_packet(1006, &format!("Failed to derive shared secret: {}", e));
            let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
            if let Err(release_err) = ip_pool.release_ip(&ip_address).await {
                warn!("Failed to release IP {}: {}", ip_address, release_err);
            }
            return Err(ServerError::KeyError(format!("Failed to derive shared secret: {}", e)));
        }
    };

    // Encrypt session key
    let (encrypted_key, key_nonce) = match crate::crypto::encryption::encrypt_session_key(
        &session_key,
        &shared_secret,
    ) {
        Ok((encrypted, nonce)) => (encrypted, nonce),
        Err(e) => {
            let error_packet = create_error_packet(1006, &format!("Encryption failed: {}", e));
            let _ = ws_sender.send(packet_to_ws_message(&error_packet)?).await;
             if let Err(release_err) = ip_pool.release_ip(&ip_address).await {
                warn!("Failed to release IP {}: {}", ip_address, release_err);
            }
            return Err(ServerError::Internal(format!("Failed to encrypt session key: {}", e)));
        }
    };

    // Create IP assignment packet
    let ip_assign = PacketType::IpAssign {
        ip_address: ip_address.clone(),
        lease_duration: ip_pool.get_default_lease_duration().as_secs(),
        session_id: session_id.clone(),
        encrypted_session_key: encrypted_key,
        key_nonce,
    };

    // Send IP assignment
    if ws_sender.send(packet_to_ws_message(&ip_assign)?).await.is_err() {
        if let Err(release_err) = ip_pool.release_ip(&ip_address).await {
             warn!("Failed to release IP {}: {}", ip_address, release_err);
        }
        return Err(ServerError::Network("Failed to send IP assignment".to_string()));
    }


    // Create the ClientSession instance using Arc<Mutex<>> for sender/receiver
    let session = ClientSession::new(
        session_id.clone(),
        public_key_string.clone(),
        ip_address.clone(),
        addr,
        Arc::new(Mutex::new(ws_sender)), // Pass Arc<Mutex<Sender>>
        Arc::new(Mutex::new(ws_receiver)), // Pass Arc<Mutex<Receiver>>
    )?;

    // Register the session
    session_manager.add_session(session.clone()).await;

    // Process client messages
    let result = process_client_session(
        session,
        key_manager,
        session_key_manager,
        packet_router,
        network_monitor,
        ip_pool.clone(), // Clone for cleanup
        session_manager.clone(), // Clone for cleanup
        server_state,
        // We don't need to pass sender/receiver anymore as they are in session
    ).await;

    // Cleanup after process_client_session finishes or errors
    info!("Cleaning up session for client {}", public_key_string);
    session_manager.remove_session(&session_id).await;
    if let Err(e) = ip_pool.release_ip(&ip_address).await {
        warn!("Failed to release IP {} during cleanup: {}", ip_address, e);
    }
    session_key_manager.remove_key(&public_key_string).await;


    result // Return the result from process_client_session
}


/// Process messages from an authenticated client session
async fn process_client_session(
    session: ClientSession,
    key_manager: Arc<KeyManager>,
    session_key_manager: Arc<SessionKeyManager>,
    packet_router: Arc<PacketRouter>,
    network_monitor: Arc<NetworkMonitor>,
    ip_pool: Arc<IpPoolManager>, // Added for cleanup
    session_manager: Arc<SessionManager>, // Added for cleanup
    server_state: Arc<RwLock<ServerState>>,
    // Removed ws_receiver and ws_sender parameters
) -> Result<(), ServerError> {
    let client_id = session.client_id.clone();
    let session_id = session.id.clone();
    let ip_address = session.ip_address.clone();
    // let _address = session.address; // Marked unused

    // --- Heartbeat Task ---
    let heartbeat_interval = Duration::from_secs(30);
    let session_hb = session.clone(); // Clone session for heartbeat task
    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = time::interval(heartbeat_interval);
        let mut sequence: u64 = 0;
        loop {
            interval.tick().await;
            let ping = PacketType::Ping {
                timestamp: current_timestamp_millis(),
                sequence,
            };
            // Use session's send_packet method
            if session_hb.send_packet(&ping).await.is_err() {
                warn!("Failed to send heartbeat to {}: channel closed", session_hb.client_id);
                break; // Exit task if sending fails
            }
            sequence = sequence.wrapping_add(1);
        }
    });

    // --- Key Rotation Task ---
    let rotation_interval = Duration::from_secs(3600); // 1 hour
    let session_rot = session.clone(); // Clone session for rotation task
    let session_key_manager_clone = session_key_manager.clone();
    let key_manager_clone = key_manager.clone();
    // Remove unused client_id_for_rotation
    let key_rotation_handle = tokio::spawn(async move {
        let mut interval = time::interval(rotation_interval);
        loop {
            interval.tick().await;

            // Use session_rot.client_id directly
            if !session_key_manager_clone.needs_rotation(&session_rot.client_id).await {
                continue;
            }

            debug!("Rotating session key for client {}", session_rot.client_id);

            let new_key = SessionKeyManager::generate_key();

             if let Some(current_key) = session_key_manager_clone.get_key(&session_rot.client_id).await {
                 match crate::crypto::encryption::encrypt_chacha20(&new_key, &current_key, None) {
                     Ok((encrypted_key, nonce)) => {
                         let key_id = random_string(16);
                         let mut sign_data = key_id.clone().into_bytes();
                         sign_data.extend_from_slice(&nonce);
                         let signature = key_manager_clone.sign_message(&sign_data).await;

                         let rotation = PacketType::KeyRotation {
                             encrypted_new_key: encrypted_key,
                             nonce,
                             key_id,
                             signature: signature.to_string(),
                         };

                        // Use session's send_packet method
                         if session_rot.send_packet(&rotation).await.is_err() {
                             warn!("Failed to send key rotation to {}: channel closed", session_rot.client_id);
                             break; // Exit task if sending fails
                         }

                        session_key_manager_clone.store_key(&session_rot.client_id, new_key).await;
                         debug!("Session key rotated for client {}", session_rot.client_id);
                     }
                     Err(e) => {
                         warn!("Failed to encrypt new session key for {}: {}", session_rot.client_id, e);
                     }
                 }
             } else {
                 warn!("Could not get current session key for rotation for client {}", session_rot.client_id);
             }
        }
    });


    let mut last_counter: Option<u64> = None;
    let mut process_result: Result<(), ServerError> = Ok(());

    // Main message processing loop
     loop {
         // Check server state first
         let current_state = *server_state.read().await;
         if current_state != ServerState::Running {
             let disconnect = create_disconnect_packet(2, "Server shutting down");
             let _ = session.send_packet(&disconnect).await; // Attempt to notify client
             process_result = Err(ServerError::Internal("Server shutting down".to_string()));
             break;
         }

        // Use session's next_message method
         match session.next_message().await {
             Some(Ok(msg)) => {
                 // Update session activity using the session method
                 session.update_activity().await;

                 match ws_message_to_packet(&msg) {
                     Ok(packet) => {
                         log_packet_info(&packet, true);

                         match packet {
                            PacketType::Data { encrypted, nonce, counter, padding: _ } => {
                                 // Check for replay attacks
                                 if let Some(last) = last_counter {
                                     if counter <= last && counter != 0 { // Allow wrap-around for u64
                                         warn!("Potential replay attack detected from {}: counter {} <= {}", client_id, counter, last);
                                         continue; // Skip processing this packet
                                     }
                                 }
                                 last_counter = Some(counter);

                                 if let Some(key) = session_key_manager.get_key(&client_id).await {
                                     match packet_router.handle_inbound_packet(&encrypted, &nonce, &key, &session).await {
                                         Ok(bytes_written) => {
                                            // Record traffic only if successful
                                             network_monitor.record_client_traffic(&client_id, 0, bytes_written as u64).await;
                                             network_monitor.record_sent(bytes_written as u64).await; // TUN write = server sending
                                         }
                                         Err(e) => {
                                             // Log routing errors but don't necessarily disconnect
                                             trace!("Failed to process inbound packet from {}: {}", client_id, e);
                                             // Consider sending an error packet back?
                                         }
                                     }
                                 } else {
                                     warn!("No session key found for client {}, dropping packet", client_id);
                                      // Consider disconnecting or sending error
                                 }
                             }
                             PacketType::Ping { timestamp, sequence } => {
                                 let pong = PacketType::Pong {
                                     echo_timestamp: timestamp,
                                     server_timestamp: current_timestamp_millis(),
                                     sequence,
                                 };
                                 // Use session's send_packet method
                                 if session.send_packet(&pong).await.is_err() {
                                     warn!("Failed to send pong to {}: channel closed", client_id);
                                     process_result = Err(ServerError::Network("Pong send failed".to_string()));
                                     break;
                                 }
                             }
                             PacketType::Pong { echo_timestamp, server_timestamp: _, sequence: _ } => {
                                 let now = current_timestamp_millis();
                                 if now >= echo_timestamp {
                                     let rtt = now - echo_timestamp;
                                     network_monitor.record_latency(&client_id, rtt as f64).await;
                                 } else {
                                      warn!("Received Pong with future timestamp from {}", client_id);
                                 }
                             }
                             PacketType::IpRenewal { session_id: renewal_id, ip_address: renewal_ip } => {
                                 if renewal_id != session_id {
                                     warn!("IP renewal with mismatched session ID from {}", client_id);
                                     continue;
                                 }
                                 if renewal_ip != ip_address {
                                     warn!("IP renewal with mismatched IP from {}", client_id);
                                     continue;
                                 }
                                 match ip_pool.renew_ip(&ip_address).await {
                                     Ok(expires_at) => {
                                         debug!("Renewed IP lease for {} until {}", client_id, expires_at);
                                         let response = PacketType::IpRenewalResponse {
                                             session_id: session_id.clone(),
                                             expires_at,
                                             success: true,
                                         };
                                          // Use session's send_packet method
                                         if session.send_packet(&response).await.is_err() {
                                              warn!("Failed to send IP renewal response to {}: channel closed", client_id);
                                             process_result = Err(ServerError::Network("IP renewal response send failed".to_string()));
                                             break;
                                         }
                                     }
                                     Err(e) => {
                                         warn!("Failed to renew IP lease for {}: {}", client_id, e);
                                         let response = PacketType::IpRenewalResponse {
                                             session_id: session_id.clone(),
                                             expires_at: 0,
                                             success: false,
                                         };
                                          // Use session's send_packet method
                                         if session.send_packet(&response).await.is_err() {
                                              warn!("Failed to send failed IP renewal response to {}: channel closed", client_id);
                                             process_result = Err(ServerError::Network("Failed IP renewal response send failed".to_string()));
                                             break;
                                         }
                                     }
                                 }
                             }
                             PacketType::Disconnect { reason, message } => {
                                 info!("Client {} disconnecting: {} (reason {})", client_id, message, reason);
                                 process_result = Ok(()); // Graceful disconnect
                                 break;
                             }
                            // Ignore other packet types received from client during session
                             _ => {
                                 warn!("Received unexpected packet type from {} during session", client_id);
                             }
                         }
                     }
                     Err(e) => { // Deserialization error
                         warn!("Failed to parse message from {}: {}", client_id, e);
                         // Decide if this warrants disconnection
                         // let error_packet = create_error_packet(1002, &format!("Invalid message: {}", e));
                         // let _ = session.send_packet(&error_packet).await;
                         // process_result = Err(ServerError::Protocol(e));
                         // break;
                     }
                 }
             }
             Some(Err(e)) => { // WebSocket error
                 debug!("WebSocket error for client {}: {}", client_id, e);
                 process_result = Err(ServerError::WebSocket(e));
                 break;
             }
             None => { // WebSocket stream closed
                 debug!("WebSocket connection closed for client {}", client_id);
                 process_result = Ok(()); // Normal closure
                 break;
             }
         }
     }


    // Abort background tasks associated with this session
    heartbeat_handle.abort();
    key_rotation_handle.abort();

    // Note: Cleanup (session removal, IP release, key removal) is now handled
    // in the `handle_client` function after this function returns or errors.

    process_result // Return the final result of the processing loop
}
