#!/usr/bin/env python3
"""Local TCP peer for async_network.cog. Handles both requests concurrently."""

import socket
import threading
import time


def handle(connection):
    with connection:
        request = bytearray()
        while True:
            chunk = connection.recv(4096)
            if not chunk:
                break
            request.extend(chunk)
        time.sleep(0.5)
        connection.sendall(b"reply: " + request)
        connection.shutdown(socket.SHUT_WR)


with socket.socket() as listener:
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 18765))
    listener.listen(2)
    print("Listening on 127.0.0.1:18765; start examples/async_network.cog", flush=True)
    threads = []
    for _ in range(2):
        connection, _ = listener.accept()
        thread = threading.Thread(target=handle, args=(connection,))
        thread.start()
        threads.append(thread)
    for thread in threads:
        thread.join()
