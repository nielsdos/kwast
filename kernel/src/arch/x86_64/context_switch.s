.extern next_thread_state
.type next_thread_state, @function

// AMD64 ABI tells us that only rbx, rbp, r12 - r15 need to be preserved by the callee.
// _switch_to_next(switch_reason)
.global _switch_to_next
.type _switch_to_next, @function
_switch_to_next:
    pushfq
    cmpl $0, %gs:8 // Check if preempt_count != 0
    jnz .flag
    pushq %rbx
    pushq %rbp
    pushq %r12
    pushq %r13
    pushq %r14
    pushq %r15

    // Protect the scheduler from nesting.
    // The interrupt flag will be restored because of the popfq later.
    cli

    movq %rsp, %rsi
    // rdi already contains `switch_reason`
    call next_thread_state
    movq %rax, %rsp
    movq %cr3, %rax
    cmpq %rax, %rdx
    jz 1f
    movq %rdx, %cr3
1:
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %rbp
    popq %rbx
    popfq

    ret
.flag:
    movb $1, %gs:12
    popfq

    ret

.global irq0
.type irq0, @function
irq0:
    pushq %rax
    pushq %rdi
    pushq %rsi
    pushq %rdx
    pushq %rcx
    pushq %r8
    pushq %r9
    pushq %r10
    pushq %r11

    // EOI, do this here because we might not end up at the bottom part if the other didn't come from an irq0.
    movb $32, %al
    outb %al, $32

    // Switch reason: regular switch (see scheduler.rs)
    xor %edi, %edi
    call _switch_to_next

    popq %r11
    popq %r10
    popq %r9
    popq %r8
    popq %rcx
    popq %rdx
    popq %rsi
    popq %rdi
    popq %rax

    iretq

.global _thread_exit
.type _thread_exit, @function
_thread_exit:
    // We want to free the memory areas of this thread. This includes the stack.
    // We can use the "interrupt stack" temporarily, because it's per-core and we are guaranteed to leave it alone
    // when the next thread is selected.
    cli
    .extern INTERRUPT_STACK_TOP
    movq $INTERRUPT_STACK_TOP, %rsp

    // Switch reason: exit (see scheduler.rs)
    movl $1, %edi
    call _switch_to_next

    // Should not get here
    ud2
