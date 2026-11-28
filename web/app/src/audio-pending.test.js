// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {audioPending, audioPendingSettled} from "./audio-pending.js";
const voice=(over)=>({server_id:"a",voice_generation:"7",settling:false,muted:true,deafened:true,...over});
test("a toggle waits until the view it was sent with is replaced",()=>{
  const toggle=audioPending(voice(),0);
  // A frame taken before the click's command was processed still shows the
  // old view: it neither ends the toggle nor names a newer one.
  assert.equal(audioPendingSettled(toggle,voice(),250),false);
  // The adapter took the change and waits for its echo.
  assert.equal(audioPendingSettled(toggle,voice({settling:true}),500),false);
  // The echo replaced the view: the next click carries this one.
  assert.equal(audioPendingSettled(toggle,voice({voice_generation:"8",deafened:false}),1500),true);
});
test("a toggle ends with its route and at its deadline",()=>{
  const toggle=audioPending(voice(),0);
  assert.equal(audioPendingSettled(toggle,null,250),true);
  assert.equal(audioPendingSettled(toggle,voice({server_id:"b"}),250),true);
  // An exhausted retry budget leaves the view where it was; the control is
  // offered again rather than staying locked.
  assert.equal(audioPendingSettled(toggle,voice(),1999),false);
  assert.equal(audioPendingSettled(toggle,voice(),2000),true);
});
test("a view without the settling flag is settled",()=>{
  const toggle=audioPending(voice({settling:undefined}),0);
  assert.equal(audioPendingSettled(toggle,{server_id:"a",voice_generation:"7"},250),true);
});
