import fetch from 'node-fetch'
import dotenv from 'dotenv'
import RPCBitcoin from 'rpc-bitcoin'
import btc from 'bitcoinjs-lib'
import fs from 'fs'

const { RPCClient } = RPCBitcoin

dotenv.config()

const mainnet = btc.networks.mainnet
const testnet = btc.networks.testnet

const is_mainnet = process.env.NETWORK === 'mainnet'
const STACKS_NETWORK = process.env.STACKS_NETWORK
const NODE_RPC_URL = process.env.NODE_RPC_URL
const BITCOIND_RPC_URL = process.env.BITCOIND_RPC_URL
const BITCOIND_RPC_PORT = process.env.BITCOIND_RPC_PORT
const BITCOIND_USER = process.env.BITCOIND_USER
const BITCOIND_PASSWORD = process.env.BITCOIND_PASSWORD

const BTC_SK = process.env.BTC_SK
const BTC_FEE_SATS_PER_BYTE = process.env.BTC_FEE_SATS_PER_BYTE
const COMMIT_SATS = parseInt(process.env.COMMIT_SATS)

function sleep(ms) {
  return new Promise(accept => {
    setTimeout(accept, ms)
  })
}

function getKeyAddress(key) {
  const { address } = btc.payments.p2pkh({
    pubkey: key.publicKey,
    network: key.network,
  })
  if (!address) {
    throw new Error('address generation failed')
  }
  return address
}

function getAccount(is_mainnet) {
  const network = is_mainnet ? mainnet : testnet
  const pkBuffer = Buffer.from(BTC_SK, 'hex').slice(0, 32)  // remove compression
  const key = btc.ECPair.fromPrivateKey(pkBuffer, { network: network })
  return { key, address: getKeyAddress(key) }
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

async function buildBlock(vrf_pk, anchored_block_hash, parent_consensus_hash, target_burn_block_height, txids) {
  try {
    const body = {
      'vrf-pk': vrf_pk,
      'anchored-block-hash': anchored_block_hash,              // stacks_tip
      'parent-consensus-hash': parent_consensus_hash,          // stacks_tip_consensus_hash
      'target-burn-block-height': target_burn_block_height,    // burn_block_height + 1
      txids,  // no transactions with [], node default behavior with `undefined`
    }
    const json = await (await fetch(
      `${NODE_RPC_URL}/v2/miner/build-block`,
      {
        method: 'post',
        body: JSON.stringify(body),
        headers: { 'Content-Type': 'application/json' },
      }
    )).text()
    return JSON.parse(json)  
  } catch(e) {
    console.log("buildBlock error", e)
    throw e
  }
}

// {
//   'block-hash': '9d19b5062b556a86f1c9bc95b041f54d6fbb77daa7e14c17e4ae67307f84c839',
//   'new-seed': '5b65cdd7d2330d81961bc7b55c3844c3c29258bc3211c4604df98a7e1eab1131',
//   'parent-block-burn-height': 0,
//   'parent-block-burn-txoff': 0,
//   recipients: [
//     { version: 26, bytes: '0000000000000000000000000000000000000000' },
//     { version: 26, bytes: '0000000000000000000000000000000000000000' }
//   ]
// }

// Wire format:
// 0      2  3            35               67     71     73    77   79     80
// |------|--|-------------|---------------|------|------|-----|-----|-----|
//  magic  op   block hash     new seed     parent parent key   key    burn_block_parent modulus
//                                          block  txoff  block txoff
// 6a4c50 OP_RETURN + OP_PUSHDATA1 0x50 (80)
// 5832 magic
// 5b op 
// 28c13e93637d6e29768f667064057d38f8b8917a9bdc88426ffeb60c90d84d08 block hash
// 1dc437134ba526439ed82e5abc574f3e867587db4599dd1a8e8c8276f712f125 new seed
// 000a39e9 tx block
// 014c tx off
// 000a384a key block
// 0703 key off
// 00 modulus

function buildCommitScript(block, target_block, vrf_key) {
  const buffer = Buffer.alloc(3 + 80)
  Buffer.from([0x6a, 0x4c, 0x50]).copy(buffer, 0)                  // OP_RETURN + OP_PUSHDATA1 0x50 (80)
  if (STACKS_NETWORK === 'mainnet') {
    Buffer.from([0x58, 0x32, 0x5b]).copy(buffer, 3)                // magic + op (mainnet)
  } else if (STACKS_NETWORK === 'xenon') {
    Buffer.from([0x58, 0x35, 0x5b]).copy(buffer, 3)                // magic + op (xenon)
  } else {
    Buffer.from([0x69, 0x64, 0x5b]).copy(buffer, 3)                // magic + op (mocknet)
  }
  Buffer.from(block['block-hash'], 'hex').copy(buffer, 3 + 3)      // block hash
  Buffer.from(block['new-seed'], 'hex').copy(buffer, 3 + 35)       // seed
  buffer.writeInt32BE(block['parent-block-burn-height'], 3 + 67)   // parent block
  buffer.writeInt16BE(block['parent-block-burn-txoff'], 3 + 71)    // parent txoff
  buffer.writeUInt32BE(vrf_key.block_height, 3 + 73)               // key block
  buffer.writeUInt16BE(vrf_key.op_vtxindex, 3 + 77)                // key txoff
  buffer.writeUint8((target_block - 1) % 5, 3 + 79)                // modulus
  return buffer
}

function getAddressFromHash(hash, network) {
  // TODO(psq): check using version 26 on testnet?  or implied?
  return btc.payments.p2pkh({network, hash: Buffer.from(hash, 'hex')}).address
}

async function buildBTCTx(client, account, utxo, block_data, target_block, vrf_key) {
  try {
    const commit_script = buildCommitScript(block_data, target_block, vrf_key)
    console.log("commit_script", commit_script.toString('hex'))

    const psbt = new btc.Psbt({ network: account.key.network })
    
    // TODO(psq)
    // psbt.addInput({
    //   hash: parent_tx.vin[0].txid,
    //   index: parent_tx.vin[0].vout,
    //   sequence: 0xFFFFFFFD,
    //   nonWitnessUtxo: Buffer.from(raw_txs[parent_tx.vin[0].txid].hex, 'hex'),
    // })

    if (!utxo.hex) {
      console.log("utxo", utxo)
      const tx = await client.getrawtransaction({ txid: utxo.txid, verbose: true })
      console.log("tx", tx)
      utxo.hex = tx.hex
    }

    psbt.addInput({
      hash: utxo.txid,
      index: utxo.vout,
      sequence: 0xFFFFFFFD,
      nonWitnessUtxo: Buffer.from(utxo.hex, 'hex'),
    })

    const fee_sat = 352 * BTC_FEE_SATS_PER_BYTE
    const change_value = Math.floor(utxo.amount * 1e8 - COMMIT_SATS - COMMIT_SATS - fee_sat)

    psbt.addOutput({ script: commit_script, value: 0 })
    psbt.addOutput({ address: getAddressFromHash(block_data.recipients[0].bytes, account.key.network), value: COMMIT_SATS })
    psbt.addOutput({ address: getAddressFromHash(block_data.recipients[0].bytes, account.key.network), value: COMMIT_SATS })
    psbt.addOutput({ address: account.address, value: change_value })  // don't keep the change, thank you, but pay the dues, no need to add parent, already contained in tx.vout[3].value

    psbt.signAllInputs(account.key)
    if (!psbt.validateSignaturesOfAllInputs()) {
      throw new Error('invalid psbt signature')
    }
    psbt.finalizeAllInputs()

    const btc_tx = psbt.extractTransaction()
    const tx_hex = btc_tx.toHex()
    const tx_id = btc_tx.getId()
    console.log("tx_id, tx_hex", tx_id, tx_hex)

    const tx_result = await client.sendrawtransaction({ hexstring: tx_hex })
    if (tx_result !== tx_id) {
      throw new Error('Calculated txid does not match txid returned from RPC')
    }
    return tx_result    
  } catch(e) {
    console.log("buildBTCTx error", e)
    return null
  }
}

async function getTxOutSet(client, address) {
  const tx_out_set = await client.scantxoutset({ action: 'start', scanobjects: [`addr(${address})`] })
  if (!tx_out_set.success) {
    console.log(`WARNING: scantxoutset did not immediately complete -- polling for progress...`);
    let scan_progress = true
    do {
      console.log("waiting")
      scan_progress = await client.scantxoutset({
        action: 'status',
        scanobjects: [`addr(${address})`],
      });
    } while (scan_progress)
    return getTxOutSet(client, address)
  }
  return tx_out_set
}


(async () => {
  console.log("is_mainnet", is_mainnet)
  const client = getRpcClient()
  const account = getAccount(is_mainnet)
  console.log("account", account.address)

  const utxos = await getTxOutSet(client, account.address)
  // console.log("utxos", utxos)
  const spendable_amount = utxos.unspents.reduce((amount, utxo) => amount + utxo.amount, 0)
  console.log("spendable_amount", spendable_amount)

  const fee = 352 * BTC_FEE_SATS_PER_BYTE
  const amount = (COMMIT_SATS * 2 + fee) * 100
  const utxo = utxos.unspents.find(utxo => Math.floor(utxo.amount * 1e8) > amount)


  const vrf_key = JSON.parse(fs.readFileSync('vrf_key.json'))
  console.log("vrf_key", vrf_key)

  const info = await getInfo()
  console.log("pox_consensus", info.pox_consensus)
  console.log("stacks_tip", info.stacks_tip)
  console.log("stacks_tip", info.burn_block_height)

  const genesis_tip = '0000000000000000000000000000000000000000000000000000000000000000'
  const genesis_consensus = '0000000000000000000000000000000000000000'

  const genesis = info.stacks_tip === genesis_tip
  const stacks_tip = genesis ? genesis_tip : info.stacks_tip
  const consensus = genesis ? genesis_consensus : info.pox_consensus

  try {
    // const result = await buildBlock(vrf_key.vrf_public_key, info.stacks_tip, info.pox_consensus, info.burn_block_height + 1, undefined)
    const block_data = await buildBlock(vrf_key.vrf_public_key, stacks_tip, consensus, info.burn_block_height + 1, undefined)
    console.log("block_data", block_data)    

    // result {
    //   'block-hash': '9d19b5062b556a86f1c9bc95b041f54d6fbb77daa7e14c17e4ae67307f84c839',
    //   'new-seed': '5b65cdd7d2330d81961bc7b55c3844c3c29258bc3211c4604df98a7e1eab1131',
    //   recipients: [
    //     { version: 26, bytes: '0000000000000000000000000000000000000000' },
    //     { version: 26, bytes: '0000000000000000000000000000000000000000' }
    //   ]
    // }
    // const script = buildCommitScript(block_data, info.burn_block_height + 1, vrf_key)
    // console.log("script", script.toString('hex'))

    const tx = await buildBTCTx(client, account, utxo, block_data, info.burn_block_height + 1, vrf_key)
    console.log("tx", tx)

  } catch(e) {
    console.log("buildBlock error", e)
  }
})()


// ERRO [1613040566.388338] [testnet/stacks-node/src/neon_node.rs:1465] [relayer] could not find header for known chain tip eef06a93aafefc065cb2928e17b19edf8bc67552 != 0000000000000000000000000000000000000000000000000000000000000000
