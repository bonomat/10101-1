import 'package:flutter/material.dart';
import 'package:get_10101/common/amount_text.dart';
import 'package:get_10101/common/amount_text_field.dart';
import 'package:get_10101/common/amount_text_input_form_field.dart';
import 'package:get_10101/common/application/channel_info_service.dart';
import 'package:get_10101/common/application/tentenone_config_change_notifier.dart';
import 'package:get_10101/common/color.dart';
import 'package:get_10101/common/dlc_channel_change_notifier.dart';
import 'package:get_10101/common/domain/model.dart';
import 'package:get_10101/common/usd_text_field.dart';
import 'package:get_10101/features/trade/channel_configuration.dart';
import 'package:get_10101/features/trade/domain/channel_opening_params.dart';
import 'package:get_10101/ffi.dart' as rust;
import 'package:get_10101/common/value_data_row.dart';
import 'package:get_10101/features/trade/domain/contract_symbol.dart';
import 'package:get_10101/features/trade/domain/direction.dart';
import 'package:get_10101/features/trade/domain/leverage.dart';
import 'package:get_10101/features/trade/domain/trade_values.dart';
import 'package:get_10101/features/trade/leverage_slider.dart';
import 'package:get_10101/features/trade/position_change_notifier.dart';
import 'package:get_10101/features/trade/submit_order_change_notifier.dart';
import 'package:get_10101/features/trade/trade_bottom_sheet_confirmation.dart';
import 'package:get_10101/features/trade/trade_dialog.dart';
import 'package:get_10101/features/trade/trade_theme.dart';
import 'package:get_10101/features/trade/trade_value_change_notifier.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';

const contractSymbol = ContractSymbol.btcusd;

class TradeBottomSheetTab extends StatefulWidget {
  final Direction direction;
  final Key buttonKey;

  const TradeBottomSheetTab({required this.direction, super.key, required this.buttonKey});

  @override
  State<TradeBottomSheetTab> createState() => _TradeBottomSheetTabState();
}

class _TradeBottomSheetTabState extends State<TradeBottomSheetTab>
    with AutomaticKeepAliveClientMixin<TradeBottomSheetTab> {
  late final TradeValuesChangeNotifier provider;
  late final TenTenOneConfigChangeNotifier tentenoneConfigChangeNotifier;
  late final PositionChangeNotifier positionChangeNotifier;

  TextEditingController marginController = TextEditingController();
  TextEditingController quantityController = TextEditingController();
  TextEditingController priceController = TextEditingController();

  final _formKey = GlobalKey<FormState>();

  bool marginInputFieldEnabled = false;
  bool quantityInputFieldEnabled = true;

  @override
  void initState() {
    provider = context.read<TradeValuesChangeNotifier>();
    provider.updateMaxQuantity();

    tentenoneConfigChangeNotifier = context.read<TenTenOneConfigChangeNotifier>();
    positionChangeNotifier = context.read<PositionChangeNotifier>();

    // init the short trade values
    final shortTradeValues = provider.fromDirection(Direction.short);
    shortTradeValues.updateQuantity(shortTradeValues.maxQuantity);
    // overwrite any potential pre-existing state
    shortTradeValues.openQuantity = Usd.zero();

    // by default we set the amount to the max quantity.
    shortTradeValues.updateContracts(shortTradeValues.maxQuantity);

    // init the long trade values
    final longTradeValues = provider.fromDirection(Direction.long);
    longTradeValues.updateQuantity(longTradeValues.maxQuantity);
    // overwrite any potential pre-existing state
    longTradeValues.openQuantity = Usd.zero();

    // by default we set the amount to the max quantity.
    longTradeValues.updateContracts(longTradeValues.maxQuantity);

    if (positionChangeNotifier.positions.containsKey(contractSymbol)) {
      // in case there is an open position we have to set the open quantity for the trade values of
      // the opposite direction
      final position = positionChangeNotifier.positions[contractSymbol]!;
      final tradeValues = provider.fromDirection(position.direction.opposite());

      tradeValues.openQuantity = position.quantity;
      tradeValues.updateQuantity(tradeValues.maxQuantity - tradeValues.openQuantity);

      // by default the contracts are set to the amount of open contracts of the current position.
      tradeValues.updateContracts(tradeValues.openQuantity);
    }

    provider.maxQuantityLock = false;

    super.initState();
  }

  @override
  void dispose() {
    marginController.dispose();
    quantityController.dispose();
    priceController.dispose();

    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    super.build(context);

    TradeTheme tradeTheme = Theme.of(context).extension<TradeTheme>()!;
    DlcChannelChangeNotifier dlcChannelChangeNotifier = context.watch<DlcChannelChangeNotifier>();

    Direction direction = widget.direction;
    String label = direction == Direction.long ? "Buy" : "Sell";
    Color color = direction == Direction.long ? tradeTheme.buy : tradeTheme.sell;

    final channelInfoService = tentenoneConfigChangeNotifier.channelInfoService;
    final channelTradeConstraints = channelInfoService.getTradeConstraints();

    final hasChannel = dlcChannelChangeNotifier.hasDlcChannel();

    return Form(
      key: _formKey,
      child: Column(
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        crossAxisAlignment: CrossAxisAlignment.center,
        mainAxisSize: MainAxisSize.min,
        children: [
          Center(
            child: Column(
              mainAxisAlignment: MainAxisAlignment.center,
              children: <Widget>[
                buildChildren(
                    direction, channelTradeConstraints, context, channelInfoService, _formKey)
              ],
            ),
          ),
          Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              ElevatedButton(
                  key: widget.buttonKey,
                  onPressed: () {
                    TradeValues tradeValues =
                        context.read<TradeValuesChangeNotifier>().fromDirection(direction);
                    if (_formKey.currentState!.validate()) {
                      final submitOrderChangeNotifier = context.read<SubmitOrderChangeNotifier>();

                      final tradeAction = hasChannel ? TradeAction.trade : TradeAction.openChannel;

                      switch (tradeAction) {
                        case TradeAction.openChannel:
                          {
                            final tradeValues =
                                context.read<TradeValuesChangeNotifier>().fromDirection(direction);
                            channelConfiguration(
                              context: context,
                              tradeValues: tradeValues,
                              onConfirmation: (ChannelOpeningParams channelOpeningParams) {
                                tradeBottomSheetConfirmation(
                                  context: context,
                                  direction: direction,
                                  tradeAction: tradeAction,
                                  onConfirmation: () => onConfirmation(
                                      submitOrderChangeNotifier, tradeValues, channelOpeningParams),
                                  channelOpeningParams: channelOpeningParams,
                                );
                              },
                            );
                            break;
                          }
                        case TradeAction.trade:
                        case TradeAction.closePosition:
                          tradeBottomSheetConfirmation(
                            context: context,
                            direction: direction,
                            tradeAction: tradeAction,
                            onConfirmation: () =>
                                onConfirmation(submitOrderChangeNotifier, tradeValues, null),
                            channelOpeningParams: null,
                          );
                      }
                    }
                  },
                  style: ElevatedButton.styleFrom(
                      backgroundColor: color, minimumSize: const Size.fromHeight(50)),
                  child: Text(
                    label,
                    style: const TextStyle(color: Colors.white),
                  )),
            ],
          )
        ],
      ),
    );
  }

  void onConfirmation(SubmitOrderChangeNotifier submitOrderChangeNotifier, TradeValues tradeValues,
      ChannelOpeningParams? channelOpeningParams) {
    submitOrderChangeNotifier.submitPendingOrder(tradeValues, PositionAction.open,
        channelOpeningParams: channelOpeningParams);

    // Return to the trade screen before submitting the pending order so that the dialog is displayed under the correct context
    GoRouter.of(context).pop();
    GoRouter.of(context).pop();

    // Show immediately the pending dialog, when submitting a market order.
    // TODO(holzeis): We should only show the dialog once we've received a match.
    showDialog(
        context: context,
        useRootNavigator: true,
        barrierDismissible: false, // Prevent user from leaving
        builder: (BuildContext context) {
          return const TradeDialog();
        });
  }

  Wrap buildChildren(Direction direction, rust.TradeConstraints channelTradeConstraints,
      BuildContext context, ChannelInfoService channelInfoService, GlobalKey<FormState> formKey) {
    final tradeValues = context.watch<TradeValuesChangeNotifier>().fromDirection(direction);
    final referralStatus = context.read<TenTenOneConfigChangeNotifier>().referralStatus;

    bool hasPosition = positionChangeNotifier.positions.containsKey(contractSymbol);

    double? positionLeverage;
    int usableBalance = channelTradeConstraints.maxLocalBalanceSats;
    if (hasPosition) {
      final position = context.read<PositionChangeNotifier>().positions[contractSymbol];
      positionLeverage = position!.leverage.leverage;
      if (direction == position.direction.opposite()) {
        usableBalance += ((position.unrealizedPnl ?? Amount.zero()) + position.collateral).sats;
      }
    }

    quantityController.text = Amount(tradeValues.contracts.usd).formatted();

    return Wrap(
      runSpacing: 12,
      children: [
        Padding(
          padding: const EdgeInsets.only(bottom: 10),
          child: Row(
            children: [
              const Flexible(child: Text("Balance:")),
              const SizedBox(width: 5),
              Flexible(child: AmountText(amount: Amount(usableBalance))),
            ],
          ),
        ),
        Selector<TradeValuesChangeNotifier, double>(
            selector: (_, provider) => provider.fromDirection(direction).price ?? 0,
            builder: (context, price, child) {
              return UsdTextField(
                value: Usd.fromDouble(price),
                label: "Market Price (USD)",
              );
            }),
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Flexible(
                child: AmountInputField(
              controller: quantityController,
              suffixIcon: TextButton(
                onPressed: () {
                  final quantity = tradeValues.maxQuantity;
                  setState(() {
                    provider.maxQuantityLock = !provider.maxQuantityLock;
                    context.read<TradeValuesChangeNotifier>().updateQuantity(direction, quantity);
                  });
                  _formKey.currentState?.validate();
                },
                child: Container(
                  padding: const EdgeInsets.all(5.0),
                  decoration: BoxDecoration(
                      borderRadius: const BorderRadius.all(Radius.circular(10)),
                      color:
                          provider.maxQuantityLock ? tenTenOnePurple.shade50 : Colors.transparent),
                  child: const Text(
                    "Max",
                    style: TextStyle(fontWeight: FontWeight.bold),
                  ),
                ),
              ),
              hint: "e.g. 100 USD",
              label: "Quantity (USD)",
              onChanged: (value) {
                Usd quantity = Usd.zero();
                try {
                  if (value.isNotEmpty) {
                    quantity = Usd.parseString(value);
                  }

                  context.read<TradeValuesChangeNotifier>().updateQuantity(direction, quantity);
                } on Exception {
                  context.read<TradeValuesChangeNotifier>().updateQuantity(direction, Usd.zero());
                }
                provider.maxQuantityLock = false;
                _formKey.currentState?.validate();
              },
              validator: (value) {
                Usd quantity = Usd.parseString(value);

                if (quantity.toInt < channelTradeConstraints.minQuantity) {
                  return "Min quantity is ${channelTradeConstraints.minQuantity}";
                }

                final maxQuantity = tradeValues.maxQuantity + tradeValues.openQuantity;
                if (quantity > maxQuantity) {
                  return "Max quantity is $maxQuantity";
                }

                return null;
              },
            )),
            const SizedBox(
              width: 10,
            ),
            Flexible(
                child: Selector<TradeValuesChangeNotifier, Amount>(
                    selector: (_, provider) =>
                        provider.fromDirection(direction).margin ?? Amount.zero(),
                    builder: (context, margin, child) {
                      return AmountTextField(
                        value: margin,
                        label: "Margin (sats)",
                      );
                    })),
          ],
        ),
        LeverageSlider(
            initialValue: positionLeverage ??
                context
                    .read<TradeValuesChangeNotifier>()
                    .fromDirection(direction)
                    .leverage
                    .leverage,
            isActive: !hasPosition,
            onLeverageChanged: (value) {
              context.read<TradeValuesChangeNotifier>().updateLeverage(direction, Leverage(value));
              formKey.currentState?.validate();
            }),
        Column(
          children: [
            Row(
              children: [
                Selector<TradeValuesChangeNotifier, double>(
                    selector: (_, provider) =>
                        provider.fromDirection(direction).liquidationPrice ?? 0.0,
                    builder: (context, liquidationPrice, child) {
                      return ValueDataRow(
                          type: ValueType.fiat, value: liquidationPrice, label: "Liquidation:");
                    }),
                const SizedBox(width: 55),
                Selector<TradeValuesChangeNotifier, Amount>(
                    selector: (_, provider) =>
                        provider.orderMatchingFee(direction) ?? Amount.zero(),
                    builder: (context, fee, child) {
                      return Flexible(
                          child: ValueDataRow(type: ValueType.amount, value: fee, label: "Fee:"));
                    }),
              ],
            ),
            if (referralStatus != null && referralStatus.referralFeeBonus > 0)
              Row(
                mainAxisSize: MainAxisSize.max,
                mainAxisAlignment: MainAxisAlignment.spaceBetween,
                children: [
                  Text(
                      "Fee rebate (${(referralStatus.referralFeeBonus * 100.0).toStringAsFixed(0)}%):",
                      style: const TextStyle(color: Colors.green)),
                  Selector<TradeValuesChangeNotifier, Amount>(
                      selector: (_, provider) =>
                          provider.orderMatchingFee(direction) ?? Amount.zero(),
                      builder: (context, fee, child) {
                        return Text(
                          "-${Amount((referralStatus.referralFeeBonus * fee.sats).floor())}",
                          style: const TextStyle(color: Colors.green),
                        );
                      }),
                ],
              ),
          ],
        ),
      ],
    );
  }

  @override
  bool get wantKeepAlive => true;
}
