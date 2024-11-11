use std::{
	collections::{BTreeMap, HashMap, HashSet},
	sync::Arc,
};

use conduit::{debug_warn, err, implement, PduEvent, Result};
use futures::{future, FutureExt};
use ruma::{
	int,
	state_res::{self},
	uint, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, ServerName,
};

use super::check_room_id;

#[implement(super::Service)]
#[allow(clippy::type_complexity)]
#[tracing::instrument(skip_all)]
pub(super) async fn fetch_prev(
	&self, origin: &ServerName, create_event: &PduEvent, room_id: &RoomId, room_version_id: &RoomVersionId,
	initial_set: Vec<Arc<EventId>>,
) -> Result<(
	Vec<Arc<EventId>>,
	HashMap<Arc<EventId>, (Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)>,
)> {
	let mut graph: HashMap<Arc<EventId>, _> = HashMap::with_capacity(initial_set.len());
	let mut eventid_info = HashMap::new();
	let mut todo_outlier_stack: Vec<Arc<EventId>> = initial_set;

	let first_pdu_in_room = self.services.timeline.first_pdu_in_room(room_id).await?;

	let mut amount = 0;

	while let Some(prev_event_id) = todo_outlier_stack.pop() {
		self.services.server.check_running()?;

		if let Some((pdu, mut json_opt)) = self
			.fetch_and_handle_outliers(origin, &[prev_event_id.clone()], create_event, room_id, room_version_id)
			.boxed()
			.await
			.pop()
		{
			check_room_id(room_id, &pdu)?;

			let limit = self.services.server.config.max_fetch_prev_events;
			if amount > limit {
				debug_warn!("Max prev event limit reached! Limit: {limit}");
				graph.insert(prev_event_id.clone(), HashSet::new());
				continue;
			}

			if json_opt.is_none() {
				json_opt = self
					.services
					.outlier
					.get_outlier_pdu_json(&prev_event_id)
					.await
					.ok();
			}

			if let Some(json) = json_opt {
				if pdu.origin_server_ts > first_pdu_in_room.origin_server_ts {
					amount = amount.saturating_add(1);
					for prev_prev in &pdu.prev_events {
						if !graph.contains_key(prev_prev) {
							todo_outlier_stack.push(prev_prev.clone());
						}
					}

					graph.insert(prev_event_id.clone(), pdu.prev_events.iter().cloned().collect());
				} else {
					// Time based check failed
					graph.insert(prev_event_id.clone(), HashSet::new());
				}

				eventid_info.insert(prev_event_id.clone(), (pdu, json));
			} else {
				// Get json failed, so this was not fetched over federation
				graph.insert(prev_event_id.clone(), HashSet::new());
			}
		} else {
			// Fetch and handle failed
			graph.insert(prev_event_id.clone(), HashSet::new());
		}
	}

	let event_fetch = |event_id| {
		let origin_server_ts = eventid_info
			.get(&event_id)
			.cloned()
			.map_or_else(|| uint!(0), |info| info.0.origin_server_ts);

		// This return value is the key used for sorting events,
		// events are then sorted by power level, time,
		// and lexically by event_id.
		future::ok((int!(0), MilliSecondsSinceUnixEpoch(origin_server_ts)))
	};

	let sorted = state_res::lexicographical_topological_sort(&graph, &event_fetch)
		.await
		.map_err(|e| err!(Database(error!("Error sorting prev events: {e}"))))?;

	Ok((sorted, eventid_info))
}