package com.honsunrise.nshext;

import java.io.BufferedReader;
import java.io.Closeable;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * A loopback HTTP server for the instrumentation tests: a page that completes, a
 * page that fails with an HTTP error, and a page whose body never arrives.
 */
final class LocalServer implements Closeable {
    private final ServerSocket listener;
    private final List<Socket> accepted = Collections.synchronizedList(new ArrayList<>());
    private final CountDownLatch slowResponse = new CountDownLatch(1);

    LocalServer() throws IOException {
        listener = new ServerSocket();
        listener.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
        Thread thread = new Thread(this::serve, "nsh-ext-test-server");
        thread.setDaemon(true);
        thread.start();
    }

    int port() {
        return listener.getLocalPort();
    }

    String url(String path) {
        return "http://127.0.0.1:" + port() + path;
    }

    boolean awaitSlowResponse(long timeoutMs) throws InterruptedException {
        return slowResponse.await(timeoutMs, TimeUnit.MILLISECONDS);
    }

    @Override
    public void close() {
        try {
            listener.close();
        } catch (IOException ignored) {
            // Closing the listener is enough to stop accepting.
        }
        synchronized (accepted) {
            for (Socket client : accepted) {
                try {
                    client.close();
                } catch (IOException ignored) {
                    // The client is going away anyway.
                }
            }
            accepted.clear();
        }
    }

    private void serve() {
        while (!listener.isClosed()) {
            Socket client;
            try {
                client = listener.accept();
            } catch (IOException stopped) {
                return;
            }
            synchronized (accepted) {
                if (listener.isClosed()) {
                    try {
                        client.close();
                    } catch (IOException ignored) {
                        // Already closed.
                    }
                    return;
                }
                accepted.add(client);
            }
            // The replacement must not wait for the retired browser's socket.
            Thread worker = new Thread(() -> serveClient(client), "nsh-ext-test-request");
            worker.setDaemon(true);
            worker.start();
        }
    }

    private void serveClient(Socket client) {
        try (Socket socket = client) {
            handle(socket);
        } catch (IOException ignored) {
            // A WebView that aborts a load can close its socket.
        } finally {
            accepted.remove(client);
        }
    }

    private void handle(Socket client) throws IOException {
        BufferedReader in = new BufferedReader(
                new InputStreamReader(client.getInputStream(), StandardCharsets.US_ASCII));
        String request = in.readLine();
        if (request == null) return;
        String line;
        while ((line = in.readLine()) != null && !line.isEmpty()) {
            // Skip the headers; the tests only need the path.
        }
        String[] parts = request.split(" ");
        String path = parts.length >= 2 ? parts[1] : "/";
        OutputStream out = client.getOutputStream();
        if ("/ok".equals(path)) {
            respond(out, 200, "OK", "<!doctype html><title>ok</title><h1>ok</h1>");
        } else if ("/a".equals(path)) {
            respond(out, 200, "OK", "<!doctype html><title>a</title><h1>a</h1>");
        } else if ("/b".equals(path)) {
            respond(out, 200, "OK", "<!doctype html><title>b</title><h1>b</h1>");
        } else if ("/missing".equals(path)) {
            respond(out, 404, "Not Found", "<!doctype html><title>missing</title>missing");
        } else if ("/redirect".equals(path)) {
            String head = "HTTP/1.1 302 Found\r\n"
                    + "Location: /ok\r\n"
                    + "Content-Length: 0\r\n"
                    + "Connection: close\r\n\r\n";
            out.write(head.getBytes(StandardCharsets.US_ASCII));
            out.flush();
        } else if ("/finish-later".equals(path)) {
            // Responds after a delay, so a replacement can begin first.
            sleep(400);
            respond(out, 200, "OK", "<!doctype html><title>late</title>late");
        } else if ("/error-later".equals(path)) {
            // No response at all: closing the socket is a network failure the engine
            // reports only after the delay.
            sleep(400);
        } else if ("/slow".equals(path)) {
            String head = "HTTP/1.1 200 OK\r\n"
                    + "Content-Type: text/html; charset=utf-8\r\n"
                    + "Content-Length: 65536\r\n"
                    + "Cache-Control: no-store\r\n"
                    + "Connection: close\r\n\r\n";
            out.write(head.getBytes(StandardCharsets.US_ASCII));
            out.flush();
            slowResponse.countDown();
            // Headers only: the document never completes. Reading blocks until the
            // WebView aborts the load (or close() closes this socket).
            while (in.read() != -1) {
                // Nothing is sent, so nothing arrives.
            }
        } else {
            respond(out, 404, "Not Found", "unknown path");
        }
    }

    private static void sleep(long millis) {
        try {
            Thread.sleep(millis);
        } catch (InterruptedException interrupted) {
            Thread.currentThread().interrupt();
        }
    }

    private static void respond(OutputStream out, int status, String reason, String body)
            throws IOException {
        byte[] bytes = body.getBytes(StandardCharsets.UTF_8);
        String head = "HTTP/1.1 " + status + " " + reason + "\r\n"
                + "Content-Type: text/html; charset=utf-8\r\n"
                + "Content-Length: " + bytes.length + "\r\n"
                + "Cache-Control: no-store\r\n"
                + "Connection: close\r\n\r\n";
        out.write(head.getBytes(StandardCharsets.US_ASCII));
        out.write(bytes);
        out.flush();
    }
}
