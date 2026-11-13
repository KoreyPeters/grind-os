# Grind OS

**An operating system that can safely run code nobody personally vouched for.**

Grind OS began as a way for people to grind drivers for their own hardware with AI: plug a machine into a helper device, let an agent iterate until the hardware works and passes its tests, then share the result in a common registry so the next person's odds improve.

Grinding forced an architecture in which AI-generated code can be run safely. That emergent property matters more than grinding itself. When AI makes code cheap to write, trust becomes the bottleneck, and Grind OS produces trust in three ways:

- **Containment.** Drivers and apps run as WebAssembly modules that can only use the capabilities (portals) they are handed.
- **Evidence.** Conformance tests, confirmations on independent machines and a transparency log make trust earned, not assumed.
- **Reversibility.** Last-known-good system images mean a mistake costs a rollback, not a disaster.

## Status

Phase 0: proving the core bet before writing a kernel. An AI agent grinds a WebAssembly Ethernet driver that reaches hardware only through capability portals, on Linux via VFIO, first in a QEMU virtual machine and then on real hardware (a Dell OptiPlex 7050 Micro).

Nothing here is usable yet.

## License

Grind OS is licensed under the GNU Affero General Public License v3.0 or later. See [LICENSE](LICENSE).

The bet: when software becomes a commodity, guaranteed openness is what stays scarce.
