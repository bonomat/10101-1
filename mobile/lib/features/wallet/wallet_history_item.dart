import 'package:flutter/material.dart';
import 'package:get_10101/common/amount_text.dart';
import 'package:get_10101/common/domain/model.dart';
import 'package:get_10101/features/wallet/domain/payment_flow.dart';
import 'package:get_10101/features/wallet/domain/wallet_history.dart';
import 'package:intl/intl.dart';
import 'package:timeago/timeago.dart' as timeago;

class WalletHistoryItem extends StatelessWidget {
  final WalletHistoryItemData data;
  static final dateFormat = DateFormat("yyyy-MM-dd HH:mm:ss");

  const WalletHistoryItem({super.key, required this.data});

  @override
  Widget build(BuildContext context) {
    const double statusIconSize = 18;
    Icon statusIcon = () {
      switch (data.status) {
        case WalletHistoryStatus.pending:
          return const Icon(
            Icons.pending,
            size: statusIconSize,
          );
        case WalletHistoryStatus.confirmed:
          return const Icon(Icons.check_circle, color: Colors.green, size: statusIconSize);
      }
    }();

    const double flowIconSize = 30;
    Icon flowIcon = () {
      if (data.type == WalletHistoryItemDataType.trade) {
        return const Icon(
          Icons.bar_chart,
          size: flowIconSize,
        );
      } else if (data.type == WalletHistoryItemDataType.orderMatchingFee ||
          data.type == WalletHistoryItemDataType.jitChannelFee) {
        return const Icon(
          Icons.toll,
          size: flowIconSize,
        );
      }

      switch (data.flow) {
        case PaymentFlow.inbound:
          return const Icon(
            Icons.arrow_downward,
            size: flowIconSize,
          );
        case PaymentFlow.outbound:
          return const Icon(Icons.arrow_upward, size: flowIconSize);
      }
    }();

    String title = () {
      switch (data.type) {
        case WalletHistoryItemDataType.lightning:
        case WalletHistoryItemDataType.onChain:
          return "Payment";
        case WalletHistoryItemDataType.trade:
          switch (data.flow) {
            case PaymentFlow.inbound:
              return "Closed position";
            case PaymentFlow.outbound:
              return "Opened position";
          }
        case WalletHistoryItemDataType.orderMatchingFee:
          return "Matching fee";
        case WalletHistoryItemDataType.jitChannelFee:
          return "Channel opening fee";
        case WalletHistoryItemDataType.stable:
          return "Stable";
      }
    }();

    String onOrOff = () {
      switch (data.type) {
        case WalletHistoryItemDataType.lightning:
        case WalletHistoryItemDataType.trade:
        case WalletHistoryItemDataType.orderMatchingFee:
        case WalletHistoryItemDataType.jitChannelFee:
          return "off-chain";
        case WalletHistoryItemDataType.onChain:
          return "on-chain";
        case WalletHistoryItemDataType.stable:
          return "stable sats";
      }
    }();

    String sign = () {
      switch (data.flow) {
        case PaymentFlow.inbound:
          return "+";
        case PaymentFlow.outbound:
          return "-";
      }
    }();

    Color color = () {
      switch (data.flow) {
        case PaymentFlow.inbound:
          return Colors.green.shade600;
        case PaymentFlow.outbound:
          return Colors.red.shade600;
      }
    }();

    var amountFormatter = NumberFormat.compact(locale: "en_UK");

    return Card(
      child: ListTile(
          onTap: () async {
            await showDialog(context: context, builder: (ctx) => showItemDetails(title, ctx));
          },
          leading: Stack(children: [
            Container(
              padding: const EdgeInsets.only(bottom: 20.0),
              child: SizedBox(height: statusIconSize, width: statusIconSize, child: statusIcon),
            ),
            Container(
                padding: const EdgeInsets.only(left: 5.0, top: 10.0),
                child: SizedBox(height: flowIconSize, width: flowIconSize, child: flowIcon)),
          ]),
          title: RichText(
            overflow: TextOverflow.ellipsis,
            text: TextSpan(
              style: DefaultTextStyle.of(context).style,
              children: <TextSpan>[
                TextSpan(text: title),
              ],
            ),
          ),
          subtitle: RichText(
              textWidthBasis: TextWidthBasis.longestLine,
              text: TextSpan(style: DefaultTextStyle.of(context).style, children: <TextSpan>[
                TextSpan(
                    text: timeago.format(data.timestamp),
                    style: const TextStyle(color: Colors.grey)),
              ])),
          trailing: Padding(
            padding: const EdgeInsets.only(top: 11.0, bottom: 5.0),
            child: Column(
              mainAxisAlignment: MainAxisAlignment.spaceBetween,
              crossAxisAlignment: CrossAxisAlignment.end,
              children: [
                RichText(
                  text: TextSpan(style: DefaultTextStyle.of(context).style, children: <InlineSpan>[
                    TextSpan(
                        text: "$sign${amountFormatter.format(data.amount.sats)} sats",
                        style: TextStyle(
                            color: color,
                            fontFamily: "Courier",
                            fontSize: 16,
                            fontWeight: FontWeight.bold))
                  ]),
                ),
                RichText(
                    text: TextSpan(style: DefaultTextStyle.of(context).style, children: <TextSpan>[
                  TextSpan(text: onOrOff, style: const TextStyle(color: Colors.grey)),
                ]))
              ],
            ),
          )),
    );
  }

  Widget showItemDetails(String title, BuildContext context) {
    List<HistoryDetail> details = () {
      switch (data.type) {
        case WalletHistoryItemDataType.lightning:
          return [HistoryDetail(label: "Payment hash", value: data.paymentHash ?? "")];
        case WalletHistoryItemDataType.onChain:
          return [HistoryDetail(label: "Transaction id", value: data.txid ?? "")];
        case WalletHistoryItemDataType.trade:
        case WalletHistoryItemDataType.stable:
        case WalletHistoryItemDataType.orderMatchingFee:
          final orderId = data.orderId!.substring(0, 8);
          return [HistoryDetail(label: "Order", value: orderId)];
        case WalletHistoryItemDataType.jitChannelFee:
          return [
            HistoryDetail(label: "Payment hash", value: data.paymentHash ?? ""),
            HistoryDetail(label: "Funding transaction id", value: data.txid ?? "")
          ];
      }
    }();

    int directionMultiplier = () {
      switch (data.flow) {
        case PaymentFlow.inbound:
          return 1;
        case PaymentFlow.outbound:
          return -1;
      }
    }();

    return AlertDialog(
      title: Text(title),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          ...details,
          HistoryDetail(
              label: "Amount", value: formatSats(Amount(data.amount.sats * directionMultiplier))),
          HistoryDetail(label: "Date and time", value: dateFormat.format(data.timestamp)),
        ],
      ),
    );
  }
}

class HistoryDetail extends StatelessWidget {
  final String label;
  final String value;

  const HistoryDetail({super.key, required this.label, required this.value});

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8.0),
      child: Row(mainAxisAlignment: MainAxisAlignment.spaceBetween, children: [
        Padding(
          padding: const EdgeInsets.only(right: 8.0),
          child: Text(label, style: const TextStyle(fontWeight: FontWeight.bold)),
        ),
        Flexible(child: Text(value)),
      ]),
    );
  }
}
