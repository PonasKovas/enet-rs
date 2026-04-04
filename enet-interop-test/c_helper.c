/*
 * c_helper.c — C-side helpers for Rust/C ENet interoperability tests.
 *
 * Two functions are exported:
 *   c_server_echo_once  — bind a server, accept one client, receive one
 *                         packet and echo it back reliably, then exit.
 *   c_client_send_recv  — connect to a server, send one packet, receive the
 *                         reply and return the payload length.
 */

#include <string.h>
#include <stdint.h>
#include "enet/enet.h"

/*
 * Run an ENet server on `port`.
 * Accepts the first client that connects, waits for one RECEIVE event, sends
 * the data back as a reliable packet, then shuts down.
 *
 * Returns 0 on success, -1 on any error or timeout.
 */
int c_server_echo_once(uint16_t port) {
    if (enet_initialize() != 0) return -1;

    ENetAddress address;
    address.host = ENET_HOST_ANY;
    address.port = port;

    ENetHost* server = enet_host_create(&address, 4 /* max peers */, 2 /* channels */, 0, 0);
    if (server == NULL) { enet_deinitialize(); return -1; }

    ENetEvent event;
    int echoed = 0;

    while (!echoed) {
        int ret = enet_host_service(server, &event, 5000);
        if (ret <= 0) break; /* timeout or error */

        switch (event.type) {
        case ENET_EVENT_TYPE_CONNECT:
            /* nothing to do, peer pointer is in event.peer */
            break;

        case ENET_EVENT_TYPE_RECEIVE:
            {
                ENetPacket* reply = enet_packet_create(
                    event.packet->data,
                    event.packet->dataLength,
                    ENET_PACKET_FLAG_RELIABLE);
                enet_peer_send(event.peer, event.channelID, reply);
                enet_host_flush(server);
                enet_packet_destroy(event.packet);
                echoed = 1;
            }
            break;

        case ENET_EVENT_TYPE_DISCONNECT:
            break;

        default:
            break;
        }
    }

    /* Drain briefly so that ACKs can be sent/received before we close. */
    {
        int i;
        for (i = 0; i < 20; i++) {
            int ret = enet_host_service(server, &event, 100);
            if (ret > 0 && event.type == ENET_EVENT_TYPE_RECEIVE)
                enet_packet_destroy(event.packet);
        }
    }

    enet_host_destroy(server);
    enet_deinitialize();
    return echoed ? 0 : -1;
}

/*
 * Connect to `host_str:port`, send `data_len` bytes from `data` on channel 0,
 * wait for one RECEIVE event and copy the payload into `out_buf`
 * (at most `out_buf_len` bytes).
 *
 * Returns the number of bytes copied into `out_buf`, or -1 on failure.
 */
int c_client_send_recv(
    const char*    host_str,
    uint16_t       port,
    const uint8_t* data,
    size_t         data_len,
    uint8_t*       out_buf,
    size_t         out_buf_len)
{
    if (enet_initialize() != 0) return -1;

    ENetHost* client = enet_host_create(NULL, 1, 2, 0, 0);
    if (client == NULL) { enet_deinitialize(); return -1; }

    ENetAddress address;
    /* Use _ip variant to avoid DNS resolution (literal IP addresses only). */
    if (enet_address_set_host_ip(&address, host_str) != 0) {
        enet_host_destroy(client);
        enet_deinitialize();
        return -1;
    }
    address.port = port;

    ENetPeer* peer = enet_host_connect(client, &address, 2, 0);
    if (peer == NULL) {
        enet_host_destroy(client);
        enet_deinitialize();
        return -1;
    }

    ENetEvent event;
    int result = -1;

    /* Wait for CONNECT */
    if (enet_host_service(client, &event, 5000) <= 0 ||
        event.type != ENET_EVENT_TYPE_CONNECT)
    {
        enet_peer_disconnect(peer, 0);
        enet_host_service(client, &event, 500);
        enet_host_destroy(client);
        enet_deinitialize();
        return -1;
    }

    /* Send our payload */
    {
        ENetPacket* packet = enet_packet_create(data, data_len, ENET_PACKET_FLAG_RELIABLE);
        enet_peer_send(peer, 0, packet);
        enet_host_flush(client);
    }

    /* Wait for the echo reply */
    while (result < 0) {
        int ret = enet_host_service(client, &event, 5000);
        if (ret <= 0) break;

        if (event.type == ENET_EVENT_TYPE_RECEIVE) {
            size_t copy_len = event.packet->dataLength < out_buf_len
                ? event.packet->dataLength
                : out_buf_len;
            memcpy(out_buf, event.packet->data, copy_len);
            result = (int)copy_len;
            enet_packet_destroy(event.packet);
        } else if (event.type == ENET_EVENT_TYPE_DISCONNECT) {
            break;
        }
    }

    enet_peer_disconnect(peer, 0);
    enet_host_service(client, &event, 500);
    enet_host_destroy(client);
    enet_deinitialize();
    return result;
}
