// The signaling service, from the connecting side.

#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "lowlat.h"
#include "matoya.h"

struct signaling {
	MTY_WebSocket *ws;
	char peer[64];
	char attempt[LOWLAT_ATTEMPT_MAX];
};

enum signaling_event {
	SIGNALING_NOTHING,
	// `theirs` holds the host's credentials.
	SIGNALING_ANSWER,
	// `candidate` holds one of the host's, or its readiness marker.
	SIGNALING_CANDIDATE,
	// The socket, the service or the host ended the attempt.
	SIGNALING_CLOSED,
};

bool signaling_connect(struct signaling *sig, const char *server, const char *session,
	const char *peer, const char *attempt);
// One socket per attempt, closed once the path is up or the attempt is given
// up; `cancel` withdraws the offer first, for an attempt that never came up.
// Closing a closed socket does nothing, and nothing is sent on one.
void signaling_close(struct signaling *sig, bool cancel);
bool signaling_offer(struct signaling *sig, const lowlat_credentials *ours);
bool signaling_candidate(struct signaling *sig, const char *address, uint16_t port, bool lan,
	bool from_stun, bool sync);
enum signaling_event signaling_poll(struct signaling *sig, uint32_t timeout_ms,
	lowlat_credentials *theirs, lowlat_candidate *candidate);
