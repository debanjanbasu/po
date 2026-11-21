// S2 — contact bootstrap: B adds A through the `po://chat` contact link.
//
// Observables (docs/device-testing.md § S2): `event=invite_sent` on the
// adder (B), `event=invite_received` on the receiver (A), and
// `event=conversation_active` on both sides.
//
// The link is built from A's OWN identity (`identity()`), in the app's real
// format (`share_urls::contact_deep_link`: `po://chat?data=<base64url(user|key)>`).
// A's invite arrives from an unknown sender, so the app parks it until A
// accepts it — `acceptInvite` is what makes the invite observable.
import { waitForSignal } from '../lib/wait.ts';
import type { RunContext } from '../lib/run.ts';
import type { Surface } from '../lib/types.ts';
import { BUDGET, PASSWORD, contactLink, pickPeers, skip } from './support.ts';

export const name = 'S2';
export const description =
  'Contact bootstrap via po://chat contact link: invite_sent, invite_received, conversation_active';

export async function run(ctx: RunContext, surfaces: Surface[]): Promise<void> {
  // A is the contact being added, B is the adder.
  const { first: receiver, second: adder } = pickPeers(surfaces);

  if (!receiver.surface.signIn || !adder.surface.signIn) {
    skip(
      ctx,
      receiver,
      'signIn',
      `both peers need their own account: ${receiver.surface.name} signIn=${receiver.surface.signIn !== undefined}, ` +
        `${adder.surface.name} signIn=${adder.surface.signIn !== undefined}`,
    );
    return;
  }
  const identity = receiver.surface.identity?.bind(receiver.surface);
  const addContact = adder.surface.addContact?.bind(adder.surface);
  if (!identity) {
    skip(ctx, receiver, 'identity', `${receiver.surface.name} cannot report the peer's own identity`);
    return;
  }
  if (!addContact) {
    skip(ctx, adder, 'addContact', `${adder.surface.name} cannot deliver a contact link`);
    return;
  }

  // Two independent accounts: a contact between two devices of the SAME
  // account would be a self-contact, which the app rejects.
  await receiver.surface.signIn?.(receiver.peer, 'e2e-s2-alpha', PASSWORD, BUDGET);
  await adder.surface.signIn?.(adder.peer, 'e2e-s2-beta', PASSWORD, BUDGET);

  // A's own identity → the link B pastes into its Add Contact dialog.
  const alpha = await identity(receiver.peer);
  const link = contactLink(alpha.username, alpha.dhtKey);
  ctx.record({
    surface: receiver.surface.name,
    peer: receiver.peer,
    kind: 'log',
    label: 'contact link built from the peer identity',
    line: `username=${alpha.username} link=${ctx.mask(link)}`,
  });

  // B adds A. The app parses the link itself and auto-submits it.
  await addContact(adder.peer, link, BUDGET);

  // The adder queues the invite once its own conversation record is live.
  const sent = await waitForSignal(
    ctx,
    adder.surface,
    adder.peer,
    { label: 'invite sent (adder)', marker: 'invite_sent' },
    BUDGET,
  );
  const conversation = sent.fields['conversation'];
  if (!conversation) {
    throw new Error(`event=invite_sent has no conversation field: ${ctx.mask(sent.line)}`);
  }

  // A receives it. An invite from an unknown sender is parked by the app, so
  // A has to accept it before the app processes it.
  const acceptInvite = receiver.surface.acceptInvite?.bind(receiver.surface);
  if (!acceptInvite) {
    skip(
      ctx,
      receiver,
      'acceptInvite',
      `${receiver.surface.name} cannot accept the pending invite; ` +
        'the app parks an invite from an unknown sender until it is accepted',
    );
    return;
  }
  await acceptInvite(receiver.peer, BUDGET);

  const received = await waitForSignal(
    ctx,
    receiver.surface,
    receiver.peer,
    { label: 'invite received (receiver)', marker: 'invite_received' },
    BUDGET,
  );
  if (received.fields['conversation'] !== conversation) {
    throw new Error(
      `the two sides disagree about the conversation\n` +
        `  adder:    ${ctx.mask(conversation)}\n` +
        `  receiver: ${ctx.mask(received.fields['conversation'] ?? '(none)')}`,
    );
  }

  // Both sides converge on an Active conversation.
  for (const ref of [adder, receiver]) {
    // oxlint-disable-next-line no-await-in-loop -- the peers may be two accounts on ONE surface, whose log reads and timeout screenshots must not interleave
    const active = await waitForSignal(
      ctx,
      ref.surface,
      ref.peer,
      { label: 'conversation active', marker: 'conversation_active' },
      BUDGET,
    );
    if (active.fields['conversation'] !== conversation) {
      throw new Error(
        `${ref.surface.name}/${ref.peer}: conversation_active names a different conversation ` +
          `(${ctx.mask(active.fields['conversation'] ?? '(none)')} vs ${ctx.mask(conversation)})`,
      );
    }
  }

  ctx.note(
    `S2 OK on ${adder.surface.name}/${adder.peer}(adder)+${receiver.surface.name}/${receiver.peer}(receiver): ` +
      `conversation ${conversation} is active on both sides`,
  );
}
