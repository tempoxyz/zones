# VSOCK proxy

`tempo-vsock-proxy` is a small, bidirectional proxy for AWS Nitro Enclaves. It
accepts TCP connections on the host, forwards their traffic to a VSOCK endpoint
inside an enclave, and sends responses back over the same TCP connection.

Run it with a host TCP port, enclave context identifier (CID), and enclave VSOCK
port:

```console
tempo-vsock-proxy <TCP_PORT> <VSOCK_CID> <VSOCK_PORT>
```

For example, the following forwards host TCP port `5000` to VSOCK port `5000`
in enclave CID `16`:

```console
tempo-vsock-proxy 5000 16 5000
```

## Why not `socat`?

Nitro Enclave connections must use the guest-to-host VSOCK transport. When a
host-to-guest VSOCK transport is also loaded, the transport must be selected by
setting `VMADDR_FLAG_TO_HOST` in the Linux `sockaddr_vm` address.

`socat`'s VSOCK address does not expose a way to set this flag, so it can select
the wrong transport or fail to connect in that configuration. This proxy opens
the VSOCK socket directly and sets `VMADDR_FLAG_TO_HOST` before connecting.

VSOCK support, including `VMADDR_FLAG_TO_HOST`, is Linux-only.
