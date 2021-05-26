use super::{BackrollError, BackrollPlayer, BackrollPlayerHandle, BackrollResult};
use crate::{
    input::{FrameInput, GameInput},
    is_null,
    protocol::{BackrollPeer, ConnectionStatus},
    sync::{self, BackrollSync},
    BackrollConfig, Frame, NetworkStats, SessionCallbacks,
};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::info;

const RECOMMENDATION_INTERVAL: Frame = 240;

pub struct P2PBackend<T>
where
    T: BackrollConfig,
{
    sync: BackrollSync<T>,
    players: Vec<BackrollPlayer<T>>,

    synchronizing: bool,
    next_recommended_sleep: Frame,
    next_spectator_frame: Frame,

    disconnect_timeout: Option<Duration>,
    disconnect_notify_start: Option<Duration>,

    local_connect_status: Arc<[RwLock<ConnectionStatus>]>,
}

impl<T: BackrollConfig> P2PBackend<T> {
    pub fn new(callbacks: Box<dyn SessionCallbacks<T>>, player_count: usize) -> Self {
        let connect_status: Vec<RwLock<ConnectionStatus>> =
            (0..player_count).map(|_| Default::default()).collect();
        let connect_status: Arc<[RwLock<ConnectionStatus>]> = connect_status.into();

        let config = sync::Config::<T> {
            callbacks,
            player_count,
        };
        let sync = BackrollSync::<T>::new(config, connect_status.clone());
        Self {
            sync,
            players: Vec::new(),
            synchronizing: true,
            next_recommended_sleep: 0,
            next_spectator_frame: 0,
            disconnect_timeout: T::DEFAULT_DISCONNECT_TIMEOUT,
            disconnect_notify_start: T::DEFAULT_DISCONNECT_NOTIFY_START,
            local_connect_status: connect_status,
        }
    }

    fn players(&self) -> impl Iterator<Item = &BackrollPeer<T>> {
        self.players
            .iter()
            .filter(|player| player.is_remote_player())
            .map(|player| player.peer())
            .flatten()
    }

    fn players_mut(&mut self) -> impl Iterator<Item = &mut BackrollPeer<T>> {
        self.players
            .iter_mut()
            .filter(|player| player.is_remote_player())
            .map(|player| player.peer_mut())
            .flatten()
    }

    fn spectators(&self) -> impl Iterator<Item = &BackrollPeer<T>> {
        self.players
            .iter()
            .filter(|player| player.is_spectator())
            .map(|player| player.peer())
            .flatten()
    }

    pub fn player_count(&self) -> usize {
        self.sync.player_count()
    }

    pub fn do_poll(&mut self) {
        if self.sync.in_rollback() || self.synchronizing {
            return;
        }

        self.sync.check_simulation();

        // notify all of our endpoints of their local frame number for their
        // next connection quality report
        let current_frame = self.sync.frame_count();
        for player in self.players_mut() {
            player.set_local_frame_number(current_frame);
        }

        let min_frame = if self.players().count() <= 2 {
            self.poll_2_players()
        } else {
            self.poll_n_players()
        };

        info!("last confirmed frame in p2p backend is {}.", min_frame);
        if min_frame >= 0 {
            debug_assert!(min_frame != Frame::MAX);
            if self.spectators().next().is_some() {
                while self.next_spectator_frame <= min_frame {
                    info!("pushing frame {} to spectators.", self.next_spectator_frame);

                    // FIXME(james7132): Spectator input sending.
                    // let (input, _)= self.sync.get_confirmed_inputs(self.next_spectator_frame);
                    // for spectator in self.spectators() {
                    //     spectator.send_input(input);
                    // }
                    self.next_spectator_frame += 1;
                }
            }
            info!("setting confirmed frame in sync to {}.", min_frame);
            self.sync.set_last_confirmed_frame(min_frame);
        }

        // send timesync notifications if now is the proper time
        if current_frame > self.next_recommended_sleep {
            let interval = self
                .players_mut()
                .map(|player| player.recommend_frame_delay())
                .max();
            if let Some(interval) = interval {
                // GGPOEvent info;
                // info.code = GGPO_EVENTCODE_TIMESYNC;
                // info.u.timesync.frames_ahead = interval;
                // _callbacks.on_event(&info);
                self.next_recommended_sleep = current_frame + RECOMMENDATION_INTERVAL;
            }
        }
    }

    fn poll_2_players(&mut self) -> Frame {
        // discard confirmed frames as appropriate
        let mut min_frame = Frame::MAX;
        for i in 0..self.players.len() {
            let player = &self.players[i];
            let mut queue_connected = true;
            if let Some(peer) = player.peer() {
                if peer.state().is_running() {
                    queue_connected = !peer.get_peer_connect_status(i).disconnected;
                }
            }
            let local_status = self.local_connect_status[i].read().unwrap().clone();
            if !local_status.disconnected {
                min_frame = std::cmp::min(local_status.last_frame, min_frame);
            }
            info!(
                "local endp: connected = {}, last_received = {}, total_min_confirmed = {}.",
                !local_status.disconnected, local_status.last_frame, min_frame
            );
            if !queue_connected && !local_status.disconnected {
                info!("disconnecting player {} by remote request.", i);
                self.disconnect_player_queue(i, min_frame);
            }
            info!("min_frame = {}.", min_frame);
        }
        return min_frame;
    }

    fn poll_n_players(&mut self) -> Frame {
        // discard confirmed frames as appropriate
        let mut min_frame = Frame::MAX;
        for queue in 0..self.players.len() {
            let mut queue_connected = true;
            let mut queue_min_confirmed = Frame::MAX;
            info!("considering queue {}.", queue);
            for (i, player) in self.players.iter().enumerate() {
                // we're going to do a lot of logic here in consideration of endpoint i.
                // keep accumulating the minimum confirmed point for all n*n packets and
                // throw away the rest.
                if player
                    .peer()
                    .map(|peer| peer.state().is_running())
                    .unwrap_or(false)
                {
                    let peer = player.peer().unwrap();
                    let status = peer.get_peer_connect_status(queue);
                    queue_connected = queue_connected && !status.disconnected;
                    queue_min_confirmed = std::cmp::min(status.last_frame, queue_min_confirmed);
                    info!("endpoint {}: connected = {}, last_received = {}, queue_min_confirmed = {}.", 
                          i, queue_connected, status.last_frame, queue_min_confirmed);
                } else {
                    info!("endpoint {}: ignoring... not running.", i);
                }
            }

            let local_status = self.local_connect_status[queue].read().unwrap().clone();
            // merge in our local status only if we're still connected!
            if !local_status.disconnected {
                queue_min_confirmed = std::cmp::min(local_status.last_frame, queue_min_confirmed);
            }
            info!(
                "local endp: connected = {}, last_received = {}, queue_min_confirmed = {}.",
                !local_status.disconnected, local_status.last_frame, queue_min_confirmed
            );

            if queue_connected {
                min_frame = std::cmp::min(queue_min_confirmed, min_frame);
            } else {
                // check to see if this disconnect notification is further back than we've been before.  If
                // so, we need to re-adjust.  This can happen when we detect our own disconnect at frame n
                // and later receive a disconnect notification for frame n-1.
                if !local_status.disconnected || local_status.last_frame > queue_min_confirmed {
                    info!("disconnecting queue {} by remote request.", queue);
                    self.disconnect_player_queue(queue, queue_min_confirmed);
                }
            }
            info!("min_frame = {}.", min_frame);
        }
        return min_frame;
    }

    pub fn add_player(
        &mut self,
        mut player: BackrollPlayer<T>,
    ) -> BackrollResult<BackrollPlayerHandle> {
        // TODO(james7132): Ensure this does not exceed maximum number of supported players.
        let handle = BackrollPlayerHandle(self.players.len());
        player.set_disconnect_timeout(self.disconnect_timeout);
        player.set_disconnect_notify_start(self.disconnect_notify_start);
        self.players.push(player);
        Ok(handle)
    }

    pub fn add_local_input(
        &mut self,
        player: BackrollPlayerHandle,
        input: FrameInput<T::Input>,
    ) -> BackrollResult<()> {
        if self.sync.in_rollback() {
            return Err(BackrollError::InRollback);
        }
        if self.synchronizing {
            return Err(BackrollError::NotSynchronized);
        }

        let queue = self.player_handle_to_queue(player)?;
        let frame = self.sync.add_local_input(queue, input.clone())?;
        if !is_null(frame) {
            for player in self.players.iter_mut() {
                player.send_input(input.clone())
            }
        }

        Ok(())
    }

    pub fn sync_input(&self) -> BackrollResult<(GameInput<T::Input>, u32)> {
        // Wait until we've started to return inputs.
        if self.synchronizing {
            return Err(BackrollError::NotSynchronized);
        }

        Ok(self.sync.synchronize_inputs())
    }

    pub fn increment_frame(&mut self) -> BackrollResult<()> {
        info!("End of frame ({})...", self.sync.frame_count());
        self.sync.increment_frame();
        self.do_poll();
        Ok(())
    }

    pub fn disconnect_player(&mut self, player: BackrollPlayerHandle) -> BackrollResult<()> {
        let queue = self.player_handle_to_queue(player)?;
        if self.local_connect_status[queue]
            .read()
            .unwrap()
            .disconnected
        {
            return Err(BackrollError::PlayerDisconnected(player));
        }

        let last_frame = self.local_connect_status[queue].read().unwrap().last_frame;
        if self.players[queue].is_local() {
            // The player is local. This should disconnect the local player from the rest
            // of the game. All other players need to be disconnected.
            // that if the endpoint is not initalized, this must be the local player.
            let current_frame = self.sync.frame_count();
            info!(
                "Disconnecting local player {} at frame {} by user request.",
                queue, last_frame
            );
            for i in 0..self.players.len() {
                if !self.players[i].is_local() {
                    self.disconnect_player_queue(i, current_frame);
                }
            }
        } else {
            info!(
                "Disconnecting queue {} at frame {} by user request.",
                queue, last_frame
            );
            self.disconnect_player_queue(queue, last_frame);
        }
        Ok(())
    }

    fn disconnect_player_queue(&mut self, queue: usize, syncto: Frame) {
        // GGPOEvent info;
        let frame_count = self.sync.frame_count();

        self.players[queue].disconnect();

        info!("Changing queue {} local connect status for last frame from {} to {} on disconnect request (current: {}).",
         queue, self.local_connect_status[queue].read().unwrap().last_frame, syncto, frame_count);

        {
            let mut status = self.local_connect_status[queue].write().unwrap();
            status.disconnected = true;
            status.last_frame = syncto;
        }

        if syncto < frame_count {
            info!(
                "Adjusting simulation to account for the fact that {} disconnected @ {}.",
                queue, syncto
            );
            self.sync.adjust_simulation(syncto);
            info!("Finished adjusting simulation.");
        }

        // info.code = GGPO_EVENTCODE_DISCONNECTED_FROM_PEER;
        // info.u.disconnected.player = QueueToPlayerHandle(queue);
        // _callbacks.on_event(&info);

        self.check_initial_sync();
    }

    pub fn get_network_stats(&self, player: BackrollPlayerHandle) -> BackrollResult<NetworkStats> {
        let queue = self.player_handle_to_queue(player)?;
        Ok(self.players[queue]
            .get_network_stats()
            .unwrap_or_else(|| Default::default()))
    }

    pub fn set_frame_delay(
        &mut self,
        player: BackrollPlayerHandle,
        delay: Frame,
    ) -> BackrollResult<()> {
        let queue = self.player_handle_to_queue(player)?;
        self.sync.set_frame_delay(queue, delay);
        Ok(())
    }

    pub fn set_disconnect_timeout(&mut self, timeout: Option<Duration>) -> BackrollResult<()> {
        self.disconnect_timeout = timeout;
        for player in self.players.iter_mut() {
            if !player.is_local() {
                player.set_disconnect_timeout(timeout);
            }
        }
        Ok(())
    }

    pub fn set_disconnect_notify_start(&mut self, timeout: Option<Duration>) -> BackrollResult<()> {
        self.disconnect_notify_start = timeout;
        for player in self.players.iter_mut() {
            if !player.is_local() {
                player.set_disconnect_notify_start(timeout);
            }
        }
        Ok(())
    }

    fn check_initial_sync(&mut self) {
        if self.synchronizing {
            // Check to see if everyone is now synchronized.  If so,
            // go ahead and tell the client that we're ok to accept input.
            for (i, player) in self.players.iter().enumerate() {
                if !player.is_local()
                    && !player.is_synchronized()
                    && !self.local_connect_status[i].read().unwrap().disconnected
                {
                    return;
                }
            }

            // GGPOEvent info;
            // info.code = GGPO_EVENTCODE_RUNNING;
            // _callbacks.on_event(&info);
            // _synchronizing = false;
        }
    }

    fn player_handle_to_queue(&self, player: BackrollPlayerHandle) -> BackrollResult<usize> {
        let offset = player.0;
        if offset >= self.player_count() {
            return Err(BackrollError::InvalidPlayer(player));
        }
        Ok(offset)
    }
}
