//! The interactive `/goal` loop's state: starting it, feeding it turns, and
//! every way of ending it, so no exit path can leave part of it behind. A loop
//! that keeps `active_goal` with nothing queued and nothing running is a status
//! line saying `active` over a loop that will never take another turn.
//!
//! What the loop decides lives in [`crate::agent::critic`]; the turn and verdict
//! handling that calls these is in [`super::runtime`].

use chrono::Utc;

use super::App;
use crate::agent::mission::{Mission, age_label};

impl App {
    /// Queue the first working turn for `/goal`, and arm the critic-gated loop:
    /// the goal becomes the standing one, and every time the builder stops an
    /// independent critic examines the work (see [`crate::agent::critic`]). The
    /// prompt lands in the transcript and the message queue, so the main loop's
    /// post-command drain starts it when the agent is idle, or right after the
    /// current turn otherwise.
    ///
    /// Takes over from any loop already running. The clock and the round count
    /// come from the saved mission, so a resumed goal keeps both.
    pub fn queue_goal_kickoff(&mut self, goal: &str) {
        self.release_goal_loop();
        let saved = Mission::load(&self.project_root)
            .ok()
            .flatten()
            .filter(|mission| mission.goal == goal);
        self.goal_since = Some(saved.as_ref().map_or_else(Utc::now, Mission::since));
        self.goal_rounds = saved.map_or(0, |mission| mission.goal_rounds);
        self.active_goal = Some(goal.to_string());
        self.persist_goal();
        self.queue_goal_turn(crate::agent::critic::goal_kickoff_prompt(goal));
    }

    /// Queue the next turn of an active `/goal` loop (a continuation, or a
    /// rework the critic asked for), arming the same post-turn handling.
    ///
    /// Ignores the queue cap. That cap is back-pressure on typing; the loop has
    /// one turn outstanding at most, and dropping it would leave a goal that
    /// reads `active` and never moves again.
    pub fn queue_goal_turn(&mut self, prompt: String) {
        self.drop_queued_goal_turn();
        self.expected_goal_prompt = Some(prompt.clone());
        self.record_prompt(prompt.clone());
        self.message_queue
            .push_back(crate::commands::Preprocessed::text_only(prompt));
    }

    /// End the live loop and keep the goal, so `/goal resume` can pick it up.
    pub fn stop_goal_loop(&mut self, why: &str) {
        if self.active_goal.is_none() {
            return;
        }
        self.note_on_mission(format!("goal loop stopped: {why}"));
        self.notice(format!(
            "goal loop stopped: {why}. `/goal resume` picks it up"
        ));
        self.end_goal_loop();
    }

    /// The critic signed off: the loop is over and the goal is met.
    pub fn finish_goal_loop(&mut self) {
        let took = self.goal_since.map(|since| age_label(since, Utc::now()));
        self.note_on_mission("goal achieved: the critic signed off");
        self.notice(match took {
            Some(took) => format!("goal achieved after {took}: the independent critic signed off."),
            None => "goal achieved: the independent critic signed off.".to_string(),
        });
        self.end_goal_loop();
    }

    /// `/goal clear`: end the live loop. The dispatcher removes the mission.
    /// A turn already running finishes unless the user stops it, and nothing
    /// follows it.
    pub fn clear_goal_loop(&mut self) {
        self.end_goal_loop();
    }

    /// Say so when the last session left a loop running, and leave it stopped.
    /// Starting it here would spend the user's money because they opened a
    /// terminal.
    pub fn announce_interrupted_goal(&mut self) {
        let Ok(Some(mut mission)) = Mission::load(&self.project_root) else {
            return;
        };
        if !mission.goal_running {
            return;
        }
        mission.goal_running = false;
        if let Err(err) = mission.save(&self.project_root) {
            tracing::warn!("could not update the mission: {err:#}");
        }
        self.notice(format!(
            "goal not running: the last session ended with it active ({} old, round {}). \
             `/goal resume` picks it up, `/goal clear` drops it:\n{}",
            age_label(mission.since(), Utc::now()),
            mission.goal_rounds,
            mission.goal,
        ));
    }

    /// Count a working turn and save it, so the round survives a restart.
    pub(super) fn count_goal_round(&mut self) {
        self.goal_rounds = self.goal_rounds.saturating_add(1);
        self.persist_goal();
    }

    /// Record a verdict on the mission, for `/goal` to show later.
    pub(super) fn note_critique(&mut self, summary: &str) {
        let summary: String = summary.chars().take(300).collect();
        let round = self.goal_rounds;
        self.note_on_mission(format!("round {round}: critic: {summary}"));
    }

    fn end_goal_loop(&mut self) {
        self.release_goal_loop();
        self.active_goal = None;
        self.goal_since = None;
        self.goal_rounds = 0;
        self.persist_goal();
    }

    /// Everything a loop holds besides the goal itself. Bumps the epoch, so a
    /// verdict already in the event channel cannot land on what comes next.
    fn release_goal_loop(&mut self) {
        self.drop_queued_goal_turn();
        self.goal_turn_running = false;
        self.goal_inflight = false;
        self.goal_idle_turns = 0;
        self.goal_critic_failures = 0;
        self.goal_epoch = self.goal_epoch.wrapping_add(1);
        if let Some(critic) = self.goal_critic.take() {
            critic.abort();
        }
    }

    fn drop_queued_goal_turn(&mut self) {
        if let Some(prompt) = self.expected_goal_prompt.take() {
            self.message_queue.retain(|queued| queued.text != prompt);
        }
    }

    fn note_on_mission(&self, note: impl Into<String>) {
        self.update_mission(|mission| mission.note(note));
    }

    /// Mirror the live loop onto the mission, for `/goal` and the next session.
    fn persist_goal(&self) {
        let running = self.active_goal.is_some();
        let rounds = self.goal_rounds;
        self.update_mission(|mission| {
            mission.goal_running = running;
            if running {
                mission.goal_rounds = rounds;
            }
        });
    }

    /// Best effort, and only on a mission that exists. Losing this bookkeeping
    /// must not stop the loop it describes.
    fn update_mission(&self, change: impl FnOnce(&mut Mission)) {
        let Ok(Some(mut mission)) = Mission::load(&self.project_root) else {
            return;
        };
        change(&mut mission);
        if let Err(err) = mission.save(&self.project_root) {
            tracing::warn!("could not update the mission: {err:#}");
        }
    }
}
