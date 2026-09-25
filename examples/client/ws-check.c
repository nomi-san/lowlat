// The toolkit's websocket reader against a server on loopback that sends
// what a peer may: a message whole, one in fragments with control frames
// between them, one whose last fragment comes late, a binary message, an
// empty one, and a close. Only whole text messages may come out, in order,
// and the ping between fragments must be answered.
//
//   make check

#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "matoya.h"

#define OP_CONTINUE 0x0
#define OP_TEXT     0x1
#define OP_BINARY   0x2
#define OP_CLOSE    0x8
#define OP_PING     0x9
#define OP_PONG     0xA
#define FIN         0x80

// An answer's length on the wire, cut where the lost ones were cut.
#define LONG_SIZE 742

static int listener = -1;
static char long_message[LONG_SIZE + 1];
static bool answered; // the server read a pong carrying its ping's payload

static void toolkit_line(const char *message, void *opaque)
{
	fprintf(stderr, "! toolkit: %s\n", message);
}

static bool send_all(int fd, const void *buf, size_t size)
{
	for (size_t at = 0; at < size;) {
		ssize_t n = send(fd, (const char *) buf + at, size - at, MSG_NOSIGNAL);
		if (n <= 0)
			return false;
		at += (size_t) n;
	}
	return true;
}

// A server frame: unmasked, one or three bytes of length.
static bool frame(int fd, uint8_t head, const void *payload, size_t size)
{
	uint8_t h[4] = {head, (uint8_t) size, 0, 0};
	size_t n = 2;
	if (size >= 126) {
		h[1] = 126;
		h[2] = (uint8_t) (size >> 8);
		h[3] = (uint8_t) size;
		n = 4;
	}
	return send_all(fd, h, n) && send_all(fd, payload, size);
}

static bool recv_all(int fd, void *buf, size_t size)
{
	for (size_t at = 0; at < size;) {
		struct pollfd p = {.fd = fd, .events = POLLIN};
		if (poll(&p, 1, 2000) <= 0)
			return false;
		ssize_t n = recv(fd, (char *) buf + at, size - at, 0);
		if (n <= 0)
			return false;
		at += (size_t) n;
	}
	return true;
}

// The upgrade: the key read from the request, the accept derived from it.
static bool upgrade(int fd)
{
	char req[4096] = {0};
	size_t len = 0;
	while (len < sizeof req - 1 && strstr(req, "\r\n\r\n") == NULL)
		if (!recv_all(fd, req + len++, 1))
			return false;

	const char *key = strstr(req, "Sec-WebSocket-Key: ");
	if (key == NULL)
		return false;
	key += strlen("Sec-WebSocket-Key: ");
	char concat[128] = {0};
	size_t klen = strcspn(key, "\r\n");
	if (klen > 64)
		return false;
	memcpy(concat, key, klen);
	strcat(concat, "258EAFA5-E914-47DA-95CA-C5AB0DC85B11");

	uint8_t sha1[MTY_SHA1_SIZE];
	MTY_CryptoHash(MTY_ALGORITHM_SHA1, concat, strlen(concat), NULL, 0, sha1, sizeof sha1);
	char accept[64];
	MTY_BytesToBase64(sha1, sizeof sha1, accept, sizeof accept);

	char res[256];
	snprintf(res, sizeof res, "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
		"Connection: Upgrade\r\nSec-WebSocket-Accept: %s\r\n\r\n", accept);
	return send_all(fd, res, strlen(res));
}

static void *serve(void *opaque)
{
	int fd = accept(listener, NULL, NULL);
	if (fd < 0 || !upgrade(fd))
		return NULL;

	const char *m = long_message;
	const uint8_t close_code[2] = {0x03, 0xE8}; // 1000
	bool sent = frame(fd, FIN | OP_TEXT, "one", 3)
		// Three fragments, a ping and a pong between them.
		&& frame(fd, OP_TEXT, m, 259)
		&& frame(fd, FIN | OP_PING, "p1", 2)
		&& frame(fd, OP_CONTINUE, m + 259, 300)
		&& frame(fd, FIN | OP_PONG, "x", 1)
		&& frame(fd, FIN | OP_CONTINUE, m + 559, LONG_SIZE - 559)
		// The last fragment late, well inside a frame's read deadline.
		&& frame(fd, OP_TEXT, "late-", 5);
	MTY_Sleep(200);
	sent = sent && frame(fd, FIN | OP_CONTINUE, "arrival", 7)
		// Neither is a text message to hand out.
		&& frame(fd, FIN | OP_BINARY, "bin", 3)
		&& frame(fd, FIN | OP_TEXT, "", 0)
		&& frame(fd, FIN | OP_TEXT, "two", 3)
		&& frame(fd, FIN | OP_CLOSE, close_code, 2);

	// What came back: masked client frames, the pong among them.
	for (int i = 0; sent && i < 8 && !answered; i++) {
		uint8_t h[2], mask[4], body[125];
		if (!recv_all(fd, h, 2) || (h[1] & 0x7F) > sizeof body)
			break;
		size_t size = h[1] & 0x7F;
		if (!recv_all(fd, mask, 4) || !recv_all(fd, body, size))
			break;
		for (size_t x = 0; x < size; x++)
			body[x] ^= mask[x % 4];
		answered = (h[0] & 0xF) == OP_PONG && size == 2 && memcmp(body, "p1", 2) == 0;
	}

	close(fd);
	return NULL;
}

int main(void)
{
	MTY_SetLogFunc(toolkit_line, NULL);
	for (size_t x = 0; x < LONG_SIZE; x++)
		long_message[x] = (char) ('a' + x % 26);

	struct sockaddr_in addr = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
	socklen_t alen = sizeof addr;
	listener = socket(AF_INET, SOCK_STREAM, 0);
	if (listener < 0 || bind(listener, (struct sockaddr *) &addr, sizeof addr) != 0 ||
		listen(listener, 1) != 0 || getsockname(listener, (struct sockaddr *) &addr, &alen) != 0) {
		fprintf(stderr, "ws-check: no loopback listener\n");
		return 1;
	}
	pthread_t server;
	if (pthread_create(&server, NULL, serve, NULL) != 0)
		return 1;

	char url[64];
	snprintf(url, sizeof url, "ws://127.0.0.1:%u/", (unsigned) ntohs(addr.sin_port));
	uint16_t status = 0;
	MTY_WebSocket *ws = MTY_WebSocketConnect(url, NULL, NULL, 2000, &status);
	if (ws == NULL) {
		fprintf(stderr, "ws-check: no connection, status %u\n", (unsigned) status);
		return 1;
	}

	const char *expected[] = {"one", long_message, "late-arrival", "two"};
	size_t count = 0;
	bool ok = true;
	MTY_Async r = MTY_ASYNC_CONTINUE;
	char msg[16 * 1024];
	for (int pass = 0; pass < 64 && r != MTY_ASYNC_DONE && r != MTY_ASYNC_ERROR; pass++) {
		r = MTY_WebSocketRead(ws, 1000, msg, sizeof msg);
		if (r != MTY_ASYNC_OK)
			continue;
		if (count >= sizeof expected / sizeof expected[0] || strcmp(msg, expected[count]) != 0) {
			fprintf(stderr, "ws-check: message %zu came out as %zu bytes: %.40s\n", count,
				strlen(msg), msg);
			ok = false;
		}
		count++;
	}
	uint16_t code = MTY_WebSocketGetCloseCode(ws);
	MTY_WebSocketDestroy(&ws);
	pthread_join(server, NULL);

	if (r != MTY_ASYNC_DONE || code != 1000) {
		fprintf(stderr, "ws-check: the read ended %d with close code %u, not at the close\n",
			(int) r, (unsigned) code);
		ok = false;
	}
	if (count != sizeof expected / sizeof expected[0]) {
		fprintf(stderr, "ws-check: %zu messages came out, not %zu\n", count,
			sizeof expected / sizeof expected[0]);
		ok = false;
	}
	if (!answered) {
		fprintf(stderr, "ws-check: the ping between fragments was not answered\n");
		ok = false;
	}
	printf("ws-check: %s\n", ok ? "ok" : "FAILED");
	return ok ? 0 : 1;
}
