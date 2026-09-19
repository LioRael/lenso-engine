# Domain context

Engine is Lenso's optional authoring layer. A work graph orders immutable-input
processing tasks; it is not an application Plugin graph. Kernel remains the owner
of Plan-bound invocation and runtime lifecycle.

Processor is an authoring implementation selected by a host. The public native
SDK lowers processors to a closed Lenso consumer/provider generation. A process
manifest describes a processor's private executable tool; it does not introduce
a new framework Execution Class or a package marketplace.

AppProject is one removable Engine processor. App source discovery, Plugin Root
authority, language/package admission, Host compilation and distribution belong
to its optional packages. The Engine core has no App or language dependencies.

Bootstrap locks pin existing local configuration and implementation artifacts.
They run before conventions and are host-local. A preset composes workflow inputs
and selected processors; it does not execute code while being inspected.

Generation is a complete set of typed resources. Publication selects one immutable
resource generation atomically. An App distribution remains governed by its
existing App-specific artifact and admission contracts.
