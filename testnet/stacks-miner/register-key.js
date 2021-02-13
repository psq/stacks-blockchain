import fetch from 'node-fetch'
import dotenv from 'dotenv'
import RPCBitcoin from 'rpc-bitcoin'
import fs from 'fs'

const { RPCClient } = RPCBitcoin

dotenv.config()

const NODE_RPC_URL = process.env.NODE_RPC_URL
const BITCOIND_RPC_URL = process.env.BITCOIND_RPC_URL
const BITCOIND_RPC_PORT = process.env.BITCOIND_RPC_PORT
const BITCOIND_USER = process.env.BITCOIND_USER
const BITCOIND_PASSWORD = process.env.BITCOIND_PASSWORD

function sleep(ms) {
  return new Promise(accept => {
    setTimeout(accept, ms)
  })
}

async function getInfo() {
    const json = await (await fetch(
    `${NODE_RPC_URL}/v2/info`,
    {
      method: 'get',
      headers: { 'Content-Type': 'application/json' },
    }
  )).text()
  return JSON.parse(json)
}

async function registerKey(consensus) {
  const body = {
    'parent-consensus-hash': consensus,
  }
  const json = await (await fetch(
    `${NODE_RPC_URL}/v2/miner/register-key`,
    {
      method: 'post',
      body: JSON.stringify(body),
      headers: { 'Content-Type': 'application/json' },
    }
  )).text()
  return JSON.parse(json)  
}

function getRpcClient() {
  const client = new RPCClient({
    url: BITCOIND_RPC_URL,
    port: parseInt(BITCOIND_RPC_PORT),
    user: BITCOIND_USER,
    pass: BITCOIND_PASSWORD,
    timeout: 120000,
  })
  return client
}

async function getTXInfo(client, txid) {
  const tx = await client.getrawtransaction({ txid, verbose: true })
  if (tx.blockhash) {
    const block = await client.getblock({ blockhash: tx.blockhash, verbose: true })
    if (block.tx) {
      const index = block.tx.indexOf(txid)
      if (index === -1) {
        return null
      }
      return {
        blockhash: tx.blockhash,
        height: block.height,
        index,
        size: tx.size,
      }
    } else {
      return null
    }
  } else {
    return null
  }
}

(async () => {
  const client = getRpcClient()

  const info = await getInfo()
  console.log("pox_consensus", info.pox_consensus)

  const key_registration = await registerKey(info.pox_consensus)
  console.log("key_registration", key_registration)

  let tx = null
  while (!tx) {
    sleep(2500)
    tx = await getTXInfo(client, key_registration.txid)
  }
  const key_registration_full = {
    vrf_public_key: key_registration['vrf-public-key'],
    block_height: tx.height,
    op_vtxindex: tx.index,
    txid: key_registration.txid,
  }
  console.log(key_registration_full)
  // this file can be used if the vrf private key is configured on the stacks-node used
  // this avoids sending the private key on the wire
  // or a key can be re-registered every time a miner starts, as the new private key will be in
  // stacks-node's memory
  fs.writeFileSync('vrf_key.json', JSON.stringify(key_registration_full))
})()
