---
name: Feature request
about: Propose a capability, subsystem, or improvement
title: "feat: "
labels: enhancement
---

## Problem / motivation

What can't be done today, or what is harder than it should be? Tie it to the project's model where you
can (the seven primitives, capabilities, the World Model, the intent→action pipeline).

## Proposed change

What you'd like to see. If it adds a new surface, describe how it stays within the capability
discipline (authorized before it acts, fail-closed, untrusted content treated as data).

## Which layer

- [ ] Hosted System-Core (`aletheia/`)
- [ ] Microkernel / HAL (`kernel*`, `kernel-core/`)
- [ ] Component runtime / SDK
- [ ] Experience layer (context, search, interfaces)
- [ ] Docs / tooling / CI

## How it would be verified

Per [ADR-010](docs/adr) (no blind code), how would this be *shown* to work — a test, a property, a VM
boot gate? A feature without a way to verify it will be treated as a design doc, not an implementation.

## Alternatives considered

Anything you weighed and rejected, and why.
