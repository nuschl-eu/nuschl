// SPDX-License-Identifier: EUPL-1.2
// The packaged web app in nix/vm-web.nix moves people between channels
// (#317). Murmur grants Move to everyone in Lobby's branch and in AFK, not in
// the root or Music. carol moves dave with the mouse (the preview is checked
// before the drop), drops him on Music to no effect, moves him back with a
// touch drag on his handle, and with the keyboard through "Move dave".
// Usage: node tests/web_move_user.browser.mjs <chromium> <playwright-core> <output directory>
import assert from "node:assert/strict";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [chromiumPath,playwright,output]=process.argv.slice(2);
assert.ok(chromiumPath&&playwright&&output,"usage: <chromium> <playwright-core> <output directory>");
const {chromium}=await import(pathToFileURL(join(playwright,"index.mjs")).href);
const browser=await chromium.launch({headless:true,executablePath:chromiumPath,
  args:["--unsafely-treat-insecure-origin-as-secure=http://web","--disable-audio-output"]});
const errors=[];
// Hang guards sized for a loaded host: within them, slowness is timing (#376).
const HANG=180000;
async function open(name,{width=1280,height=800,touch=false}={}) {
  const context=await browser.newContext({viewport:{width,height},hasTouch:touch});
  const page=await context.newPage();
  page.setDefaultTimeout(HANG);page.setDefaultNavigationTimeout(HANG);
  page.on("pageerror",error=>errors.push(`${name}: ${error.message}`));
  await page.addInitScript(()=>{
    const proof=window.__move={servers:[],moves:[]};
    const Worker=window.Worker;
    window.Worker=class extends Worker{
      constructor(url,options){super(url,options);this.addEventListener("message",({data})=>{if(data.kind==="state")proof.servers=data.servers;});}
      postMessage(message,...rest){if(message?.method==="move_user")proof.moves.push(message.args);return super.postMessage(message,...rest);}
    };
  });
  await page.goto(`http://web/app/#s=murmur:64738&u=${name}`);
  // On a phone the server and its tree are in the navigation drawer.
  if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null);
  return {page,context};
}
// dave's channel as carol's page shows it.
const channelOf=page=>page.evaluate(()=>__move.servers[0]?.users.find(user=>user.name==="dave")?.channel);
const channelId=(page,name)=>page.evaluate(name=>__move.servers[0].channels.find(channel=>channel.name===name)?.id,name);
const centre=async locator=>{const box=await locator.boundingBox();return [box.x+box.width/2,box.y+box.height/2];};
try {
  const {page:dave}=await open("dave");
  await dave.getByRole("button",{name:"Lobby",exact:true}).click();
  await dave.getByRole("button",{name:"Join voice",exact:true}).click();
  const {page:carol}=await open("carol");
  const [lobby,afk]=[await channelId(carol,"Lobby"),await channelId(carol,"AFK")];
  await carol.waitForFunction(lobby=>__move.servers[0]?.users.find(user=>user.name==="dave")?.channel===lobby,lobby);
  // Move is known in Lobby once the SDK asked: the row becomes draggable.
  const daveRow=carol.getByRole("button",{name:"Private message dave",exact:true});
  await carol.locator(".user-slot.movable").filter({has:daveRow}).waitFor();
  const afkButton=carol.getByRole("button",{name:"AFK",exact:true}),music=carol.getByRole("button",{name:"Music",exact:true});

  // The mouse: the preview shows dave under AFK before anything is sent.
  await carol.mouse.move(...await centre(daveRow));await carol.mouse.down();
  const [x,y]=await centre(afkButton);
  await carol.mouse.move(x,y,{steps:10});
  await carol.locator(".channel-row.move-target").filter({has:afkButton}).waitFor();
  await carol.waitForTimeout(300);
  assert.ok((await daveRow.boundingBox()).y>(await afkButton.boundingBox()).y,"no preview of dave under AFK");
  assert.equal(await channelOf(carol),lobby,"the preview moved dave before the drop");
  await carol.screenshot({path:join(output,"web-move-preview.png")});
  await carol.mouse.up();
  await carol.waitForFunction(afk=>__move.servers[0].users.find(user=>user.name==="dave")?.channel===afk,afk);
  await carol.locator(".channels").screenshot({path:join(output,"web-move-dropped.png")});

  // Music grants no Move: no target, no command.
  const sent=(await carol.evaluate(()=>__move.moves.length));
  await daveRow.dragTo(music);
  await carol.waitForTimeout(1000);
  assert.equal(await carol.evaluate(()=>__move.moves.length),sent,"a drop on Music sent a move");
  assert.equal(await channelOf(carol),afk);

  // Touch: carol on a phone drags dave's handle back to Lobby.
  const {page:phone,context:touch}=await open("carol-phone",{width:390,height:844,touch:true});
  const handle=phone.getByRole("button",{name:"Move dave",exact:true});
  await handle.waitFor();
  const cdp=await touch.newCDPSession(phone);
  const [hx,hy]=await centre(handle),[lx,ly]=await centre(phone.getByRole("button",{name:"Lobby",exact:true}));
  await cdp.send("Input.dispatchTouchEvent",{type:"touchStart",touchPoints:[{x:hx,y:hy,id:1}]});
  for(let step=1;step<=8;step++)await cdp.send("Input.dispatchTouchEvent",{type:"touchMove",touchPoints:[{x:hx+(lx-hx)*step/8,y:hy+(ly-hy)*step/8,id:1}]});
  const ghost=phone.locator(".user-move-ghost");await ghost.waitFor();
  const box=await ghost.boundingBox();
  assert.ok(Math.abs(box.x+box.width/2-lx)<2&&Math.abs(box.y+box.height/2-ly)<2,"the touch ghost is not under the finger");
  await phone.screenshot({path:join(output,"web-move-touch.png")});
  await cdp.send("Input.dispatchTouchEvent",{type:"touchEnd",touchPoints:[]});
  await carol.waitForFunction(lobby=>__move.servers[0].users.find(user=>user.name==="dave")?.channel===lobby,lobby);
  await touch.close();

  // The keyboard: dave's handle opens "Move dave"; Enter on AFK moves him.
  const keyHandle=carol.getByRole("button",{name:"Move dave",exact:true});
  await keyHandle.focus();await carol.keyboard.press("Enter");
  const dialog=carol.getByRole("dialog",{name:"Move dave"});
  await dialog.waitFor();
  assert.equal(await dialog.getByRole("button",{name:"Music",exact:true}).count(),0,"Music is offered without Move");
  await dialog.getByRole("button",{name:"AFK",exact:true}).focus();
  await carol.keyboard.press("Enter");
  await carol.waitForFunction(afk=>__move.servers[0].users.find(user=>user.name==="dave")?.channel===afk,afk);
  assert.deepEqual(errors,[]);
  console.log("move: mouse with preview, no Move in Music, touch handle and keyboard all move dave as the server allows");
} finally {
  await browser.close();
}
